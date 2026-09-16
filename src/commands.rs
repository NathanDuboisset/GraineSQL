//! Command implementations, the layer that wires config, database, lock, and
//! formats together.

use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use indexmap::IndexMap;

use crate::cli::{Cli, Command};
use crate::cmd::buckets::{
    export_buckets, human_bytes, live_bucket_settings, load_buckets, storage_client,
};
use crate::cmd::scaffold::{cmd_add, cmd_completions, cmd_init, cmd_status};
use crate::config::{Config, ResolvedTable};
use crate::db::{self, Db};
use crate::export;
use crate::format;
use crate::load;
use crate::lock::{FileEntry, Lock, drift};
use crate::order;
use crate::schema::{Schema, TableId};
use crate::source::{self, ResolvedSource};
use crate::storage::{self, BucketExport};

/// Shared setup: config, resolved source, and reporting settings.
pub struct Ctx {
    pub cfg: Config,
    pub cli_source: Option<String>,
    pub json: bool,
    pub verbose: bool,
    pub quiet: bool,
    /// Skip every confirmation prompt.
    pub assume_yes: bool,
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
            assume_yes: cli.yes,
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

    /// Ask a yes/no question, honouring `--yes`.
    pub fn confirm(&self, prompt: &str) -> Result<bool> {
        if self.assume_yes {
            self.warn(format!("{prompt} yes (--yes)"));
            return Ok(true);
        }
        confirm(prompt)
    }

    /// Ask a drift question, which also offers "and stop asking". `--yes`
    /// accepts without remembering: it is a CI switch, not a decision.
    pub fn confirm_drift(&self, prompt: &str, scope: &str) -> Result<Answer> {
        if self.assume_yes {
            self.warn(format!("{prompt} yes (--yes)"));
            return Ok(Answer::Once);
        }
        confirm_drift(prompt, scope)
    }
}

/// What the user said to one drift prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Once,
    No,
    Always,
}

/// Introspect the live schema, pruned to the configured tables and their
/// foreign-key closure.
pub async fn introspect(db: &Db, cfg: &Config) -> Result<Schema> {
    let full = db::introspect(db).await?;
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
                .filter(|id| lock.to_schema_for(&live).resolve(id).is_none())
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
    gate_on_drift_with_buckets(ctx, lock, live, &IndexMap::new(), force)
}

/// Same, additionally comparing storage bucket settings.
///
/// `live_buckets` is empty when the config declares no buckets, in which case
/// this behaves exactly like [`gate_on_drift`].
pub fn gate_on_drift_with_buckets(
    ctx: &Ctx,
    lock: &Lock,
    live: &Schema,
    live_buckets: &IndexMap<String, storage::BucketSettings>,
    force: bool,
) -> Result<drift::Report> {
    let creatable = if lock.engine.is_sql() {
        drift::Creatable::No
    } else {
        drift::Creatable::Yes
    };
    let mut report = drift::classify_with(&lock.to_schema_for(live), live, creatable);

    if !lock.buckets.is_empty() || !live_buckets.is_empty() {
        let largest: IndexMap<String, u64> = lock
            .bucket_files
            .iter()
            .map(|(name, entry)| (name.clone(), entry.bytes))
            .collect();
        report.drifts.extend(drift::classify_buckets(
            &lock.buckets,
            live_buckets,
            &largest,
        ));
        report.drifts.sort_by(|a, b| {
            a.severity
                .cmp(&b.severity)
                .then_with(|| a.target.cmp(&b.target))
                .then_with(|| a.what.cmp(&b.what))
        });
    }

    // A per-table `on_drift: ignore` drops the change before anything acts on
    // it, which is the point: it must not need a blanket --force.
    let policy = drift_policy(ctx)?;
    report.drifts.retain(|d| {
        table_policy(&policy, d, &live.default_schema) != crate::config::OnDrift::Ignore
    });

    if report.is_empty() {
        return Ok(report);
    }

    let hard = report.drifts.iter().any(|d| {
        d.severity == drift::Severity::Breaking
            || (d.severity == drift::Severity::Confirm
                && table_policy(&policy, d, &live.default_schema) == crate::config::OnDrift::Abort)
    });

    if hard && !force {
        bail!(
            "schema drift vs graine.lock\n{}\nNothing was changed. Review, then run \
             `graine lock` to accept, or --force to proceed anyway.",
            report.render()
        );
    }

    ctx.warn(format!("schema drift vs graine.lock\n{}", report.render()));

    if hard {
        ctx.warn("proceeding past breaking drift because --force was given");
        return Ok(report);
    }

    // One prompt per change, so accepting the loss of one column is never taken
    // as accepting the loss of another.
    let mut remember: Vec<(String, String)> = Vec::new();
    for d in report.confirm() {
        let (scope, change) = d.accept_entry(&live.default_schema);
        if lock.is_accepted(&scope, &change) {
            ctx.detail(format!("  {scope}: {change} (accepted in graine.lock)"));
            continue;
        }
        match ctx.confirm_drift(&format!("{}: {}.", d.target, d.what), &scope)? {
            Answer::Once => {}
            Answer::No => bail!("declined {} ({}); nothing was changed", d.target, d.what),
            Answer::Always => remember.push((scope, "*".to_string())),
        }
    }

    if !remember.is_empty() {
        persist_accepted(ctx, &remember)?;
    }
    Ok(report)
}

/// Per-table drift policy, keyed by resolved table id.
fn drift_policy(ctx: &Ctx) -> Result<Vec<(TableId, crate::config::OnDrift)>> {
    Ok(ctx
        .cfg
        .resolved_tables()?
        .into_iter()
        .map(|t| (t.id, t.on_drift))
        .collect())
}

fn table_policy(
    policy: &[(TableId, crate::config::OnDrift)],
    d: &drift::Drift,
    default_schema: &str,
) -> crate::config::OnDrift {
    d.target
        .table()
        .and_then(|t| {
            policy
                .iter()
                .find(|(id, _)| id.matches(t, default_schema))
                .map(|(_, p)| *p)
        })
        .unwrap_or(crate::config::OnDrift::Confirm)
}

/// Write newly accepted drift back into the lock.
fn persist_accepted(ctx: &Ctx, entries: &[(String, String)]) -> Result<()> {
    let path = ctx.cfg.lock_path();
    let mut lock = Lock::read(&path)?;
    for (scope, change) in entries {
        lock.accept(scope, change);
    }
    lock.write(&path)?;
    for (scope, _) in entries {
        ctx.say(format!(
            "remembered: drift on {scope} will not be asked about again (recorded in {})",
            path.display()
        ));
    }
    Ok(())
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
        let report = drift::classify(&existing.to_schema_for(&live), &live);
        if ctx.json {
            println!("{}", drift_json(&report));
        } else {
            print!("{}", report.render());
        }
        if !report.is_empty() {
            bail!(
                "graine.lock is out of date with source {:?}; run `graine lock` to update it",
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
        let scopes: BTreeSet<String> = lock.schema.keys().map(|id| id.to_string()).collect();
        lock.carry_accepted(prev, |s| scopes.contains(s) || s.starts_with("bucket "));
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
            let report = drift::classify(&prev.to_schema_for(&live), &live);
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

pub async fn cmd_diff(
    ctx: &Ctx,
    data: bool,
    full: bool,
    tables: Option<Vec<String>>,
    forget: Vec<String>,
    forget_all: bool,
) -> Result<()> {
    let path = ctx.cfg.lock_path();

    if forget_all || !forget.is_empty() {
        let mut lock = Lock::read(&path)?;
        let before = lock.accepted.len();
        if forget_all {
            lock.accepted.clear();
        } else {
            lock.accepted
                .retain(|scope, _| !forget.iter().any(|f| f == scope));
        }
        let dropped = before - lock.accepted.len();
        lock.write(&path)?;
        ctx.say(format!(
            "forgot accepted drift on {dropped} table{}",
            if dropped == 1 { "" } else { "s" }
        ));
        return Ok(());
    }

    let (_src, db) = ctx.connect(1).await?;
    let live = introspect(&db, &ctx.cfg).await?;
    let lock = Lock::read(&path)?;
    let report = drift::classify(&lock.to_schema_for(&live), &live);

    if data {
        // Drift first: comparing rows against a schema that moved would report
        // differences that are really the schema change showing through.
        if report.has_breaking() {
            bail!(
                "schema drift vs graine.lock\n{}\nThe rows cannot be compared until \
                 this is resolved.",
                report.render()
            );
        }
        let diffs = data_diff(ctx, &db, &live, &lock, tables).await?;
        if ctx.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&crate::diff::to_json(&diffs))?
            );
        } else {
            print!("{}", crate::diff::render(&diffs, full));
        }
        return Ok(());
    }

    if ctx.json {
        println!("{}", drift_json(&report));
    } else {
        print!("{}", report.render());
        if !lock.accepted.is_empty() {
            println!("\naccepted (in {}, `--forget` to undo):", path.display());
            for (scope, changes) in &lock.accepted {
                for c in changes {
                    let what = if c == "*" { "all drift" } else { c };
                    println!("  {scope:<28}  {what}");
                }
            }
        }
    }

    if report.has_breaking() {
        std::process::exit(1);
    }
    Ok(())
}

/// Compare every selected table's seed rows against the live ones.
async fn data_diff(
    ctx: &Ctx,
    db: &Db,
    live: &Schema,
    lock: &Lock,
    tables: Option<Vec<String>>,
) -> Result<Vec<crate::diff::TableDiff>> {
    let out_dir = ctx.cfg.out_dir();
    let selected = ctx.cfg.select_tables(tables.as_deref())?;
    let order = compute_order(live, &ctx.cfg)?;
    let batch = crate::diff::chunk_for(db.engine());

    let mut diffs = Vec::new();
    for id in &order {
        let Some(cfg) = selected
            .iter()
            .find(|t| live.resolve(&t.id).as_ref() == Some(id) || t.id == *id)
        else {
            continue;
        };
        let table = live
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("table {id} is not in the live schema"))?;
        let columns = export::selected_columns(table, cfg)?;
        let rows = format::read(&out_dir, &columns, cfg, &live.default_schema, lock.engine)?;
        diffs.push(crate::diff::table_diff(db, table, &columns, &rows, cfg, batch).await?);
    }
    Ok(diffs)
}

/// Load into a document engine, which has no SQL and no foreign keys.
#[allow(unused_variables)]
async fn load_documents(
    ctx: &Ctx,
    db: &Db,
    live: &Schema,
    loads: &[(ResolvedTable, Vec<Vec<crate::value::Value>>)],
    dry_run: bool,
    use_transaction: bool,
) -> Result<()> {
    #[cfg(not(feature = "mongo"))]
    bail!("this build has no MongoDB support");

    #[cfg(feature = "mongo")]
    {
        // A multi-document transaction needs a replica set or mongos. Failing
        // here rather than degrading silently keeps the promise that a failed
        // load leaves the database as it was.
        if use_transaction && !crate::db::mongo::ops::supports_transactions(db).await? {
            bail!(
                "source {:?} is a standalone mongod, which cannot run a multi-document \
                 transaction, so a failure part-way through would leave a partial load.\n\
                 Start it with --replSet, or pass --no-transaction to accept that.",
                db.source_name
            );
        }
        if dry_run {
            ctx.say("dry run: nothing was written");
            return Ok(());
        }

        let mut total = 0u64;
        for (cfg, rows) in loads {
            let r = crate::db::mongo::data::load_collection(db, live, cfg, rows).await?;
            ctx.detail(format!(
                "  {} -> {} document{}",
                cfg.id,
                r.affected,
                if r.affected == 1 { "" } else { "s" }
            ));
            total += r.rows;
        }
        ctx.say(format!(
            "loaded {} document{} into {} collection{}",
            total,
            if total == 1 { "" } else { "s" },
            loads.len(),
            if loads.len() == 1 { "" } else { "s" }
        ));
        Ok(())
    }
}

fn drift_json(report: &drift::Report) -> String {
    let items: Vec<serde_json::Value> = report
        .drifts
        .iter()
        .map(|d| {
            serde_json::json!({
                "severity": d.severity.label(),
                "target": d.target.to_string(),
                "table": d.target.table().map(|t| t.to_string()),
                "column": d.target.column(),
                "bucket": d.target.bucket(),
                "change": d.what,
                "note": d.note,
            })
        })
        .collect();
    let (breaking, confirm, benign) = report.counts();
    serde_json::to_string_pretty(&serde_json::json!({
        "breaking": breaking,
        "needs_confirmation": confirm,
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
    no_fk_check: bool,
    follow_parents: bool,
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
        let live_buckets = if no_buckets {
            IndexMap::new()
        } else {
            live_bucket_settings(ctx, &_src, buckets.as_deref()).await?
        };
        gate_on_drift_with_buckets(ctx, lock, &live, &live_buckets, force)?;
    } else if !force {
        ctx.warn(format!(
            "note: no lock file at {} yet; it will be created by this export",
            ctx.cfg.lock_path().display()
        ));
    }

    // A configured table the database no longer has was reported as drift and
    // accepted above; skip it rather than failing on it here.
    let order: Vec<TableId> = order
        .into_iter()
        .filter(|id| {
            if live.get(id).is_some() {
                return true;
            }
            ctx.warn(format!("skipping {id}: it no longer exists"));
            false
        })
        .collect();

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

    // Which key values to capture, so the referential check can run afterwards.
    let exported_ids: Vec<TableId> = ordered
        .iter()
        .map(|t| live.resolve(&t.id).unwrap_or_else(|| t.id.clone()))
        .collect();
    // Skipping the check means the key index is dead weight, and it is the one
    // thing still held for every table for the whole run.
    let to_index = if no_fk_check {
        std::collections::BTreeMap::new()
    } else {
        export::columns_to_index(&live, &exported_ids)
    };
    let mut exports: std::collections::BTreeMap<TableId, export::TableExport> =
        std::collections::BTreeMap::new();

    // Resolved before anything is written: the pulled rows have to arrive under
    // the same ORDER BY as the rest, not be appended afterwards.
    let pulled = if follow_parents || ctx.cfg.export.follow_parents {
        let (pulled, pulls) =
            crate::closure::resolve(&db, &live, &ordered, crate::closure::DEFAULT_MAX_DEPTH)
                .await?;
        let rendered = crate::closure::render(&pulls);
        if !rendered.is_empty() {
            ctx.say(format!("following foreign keys:\n{rendered}"));
        }
        pulled
    } else {
        crate::closure::Pulled::new()
    };

    let mut progress = crate::progress::Progress::new(ctx, ordered.len());

    for cfg in &ordered {
        let id = live.resolve(&cfg.id).unwrap_or_else(|| cfg.id.clone());
        let index = to_index.get(&id).cloned().unwrap_or_default();
        progress.step(&id.to_string());
        let ticker = std::cell::RefCell::new(&mut progress);
        let exported = if db.engine().is_sql() {
            export::export_table(
                &db,
                &live,
                cfg,
                ctx.cfg.export.sql_batch,
                &index,
                pulled.get(&id),
                &out_dir,
                |rows| ticker.borrow_mut().rows(rows),
            )
            .await?
        } else {
            #[cfg(feature = "mongo")]
            {
                crate::db::mongo::data::export_collection(&db, &live, cfg, &out_dir, |rows| {
                    ticker.borrow_mut().rows(rows)
                })
                .await?
            }
            #[cfg(not(feature = "mongo"))]
            bail!("this build has no MongoDB support")
        };
        ctx.detail(format!(
            "  [{}/{}] {} -> {} ({} rows)",
            written.len() + 1,
            ordered.len(),
            cfg.id,
            exported.paths.join(", "),
            exported.rows
        ));
        written.extend(exported.paths.iter().map(|p| out_dir.join(p)));
        total_rows += exported.rows;

        files.insert(
            crate::lock::relative(&exported.table, &live.default_schema),
            FileEntry {
                path: export::lock_path(&exported, cfg, &live.default_schema),
                rows: exported.rows,
                sha256: exported.sha256.clone(),
            },
        );
        summary.push(serde_json::json!({
            "table": exported.table.to_string(),
            "rows": exported.rows,
            "files": exported.paths,
        }));
        exports.insert(exported.table.clone(), exported);
    }
    progress.clear();

    // A set of per-table filters can easily orphan rows. Loading that into an
    // empty database fails partway through, so say so now instead.
    if !no_fk_check {
        let dangling = export::check_referential_closure(&live, &exports);
        if !dangling.is_empty() {
            bail!("{}", export::render_dangling(&dangling));
        }
    }

    // Buckets: the half of a project a SQL dump cannot capture.
    let (bucket_settings, bucket_entries, bucket_summary) =
        export_buckets(ctx, &_src, &out_dir, buckets.as_deref()).await?;

    // A table removed from the config leaves an orphan file behind, which would
    // then load stale data forever. Remove ours, never anything else.
    let removed = crate::io::prune_stale(&out_dir, &written)?;
    for path in &removed {
        ctx.detail(format!("  removed stale {}", path.display()));
    }

    let mut lock = Lock::build(db.engine(), &live, &order);
    lock.files = files;
    lock.buckets = bucket_settings;
    lock.bucket_files = bucket_entries;
    // Export rebuilds the lock from scratch, so without this an acceptance
    // recorded during a load is silently dropped by the next export.
    if let Ok(prev) = Lock::read(&lock_path) {
        let scopes: BTreeSet<String> = lock.schema.keys().map(|id| id.to_string()).collect();
        lock.carry_accepted(&prev, |s| scopes.contains(s) || s.starts_with("bucket "));
    }
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
        let objects: u64 = lock.bucket_files.values().map(|b| b.objects).sum();
        ctx.say(format!(
            "exported {} table{}, {total_rows} row{}{} to {}",
            ordered.len(),
            if ordered.len() == 1 { "" } else { "s" },
            if total_rows == 1 { "" } else { "s" },
            if lock.bucket_files.is_empty() {
                String::new()
            } else {
                format!(
                    ", {} bucket{} ({objects} object{}, {})",
                    lock.bucket_files.len(),
                    if lock.bucket_files.len() == 1 {
                        ""
                    } else {
                        "s"
                    },
                    if objects == 1 { "" } else { "s" },
                    human_bytes(lock.bucket_files.values().map(|b| b.bytes).sum())
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
// plan / load
// ---------------------------------------------------------------------------

/// Read every selected table's rows and build the load plan.
#[allow(clippy::type_complexity)]
async fn build_plans(
    ctx: &Ctx,
    db: &Db,
    live: &Schema,
    live_buckets: &IndexMap<String, storage::BucketSettings>,
    tables: Option<Vec<String>>,
    force: bool,
) -> Result<(
    Vec<(ResolvedTable, Vec<Vec<crate::value::Value>>)>,
    Vec<load::TablePlan>,
)> {
    let selected = ctx.cfg.select_tables(tables.as_deref())?;
    let lock = Lock::read(&ctx.cfg.lock_path())?;
    gate_on_drift_with_buckets(ctx, &lock, live, live_buckets, force)?;

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
        // A collection the target has not got yet is created by the load, so
        // the lock is the only description of it there is.
        let locked = lock.to_schema_for(live);
        let table = live
            .get(id)
            .or_else(|| locked.get(id))
            .ok_or_else(|| anyhow::anyhow!("table {id} is not in the live schema"))?;
        let columns = export::selected_columns(table, cfg)?;
        let rows = format::read(&out_dir, &columns, cfg, &live.default_schema, lock.engine)?;
        load::validate_rows(table, &columns, &rows, cfg)?;

        let key = load::upsert_key(table, cfg)?;
        // Rows outside the predicate get no arbiter, so they plain-insert and
        // collide with the primary key on the second load.
        if cfg.load_mode == crate::config::LoadMode::Upsert
            && !table.primary_key.is_empty()
            && table.primary_key != key
            && table.conflict_target(&key).is_some_and(|u| u.is_partial())
        {
            ctx.warn(format!(
                "{id}: `key: [{}]` is a partial unique index, so rows outside its \
                 WHERE are inserted rather than upserted and a second load of them \
                 will fail on the primary key",
                key.join(", ")
            ));
        }

        plans.push(load::TablePlan {
            table: id.clone(),
            mode: cfg.load_mode,
            rows: rows.len() as u64,
            columns: columns.iter().map(|c| c.name.clone()).collect(),
            key,
        });
        loads.push((cfg.clone(), rows));
    }

    let _ = db;
    Ok((loads, plans))
}

pub async fn cmd_plan(ctx: &Ctx, tables: Option<Vec<String>>, tree: bool) -> Result<()> {
    let (src, db) = ctx.connect(1).await?;
    let live = introspect(&db, &ctx.cfg).await?;
    // Without the live bucket settings the drift check has nothing to compare
    // against and would report every configured bucket as missing.
    let live_buckets = live_bucket_settings(ctx, &src, None).await?;
    let (_loads, plans) = build_plans(ctx, &db, &live, &live_buckets, tables, false).await?;

    if !ctx.json && tree {
        print!("{}", load::render_tree(&db.source_name, &plans, &live));
        return Ok(());
    }

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
    force: bool,
) -> Result<()> {
    let src = ctx.resolve_source()?;
    load::check_writable(&src)?;

    let db = Db::connect_with(&src, 1).await?;
    let live = introspect(&db, &ctx.cfg).await?;
    let live_buckets = if no_buckets {
        IndexMap::new()
    } else {
        live_bucket_settings(ctx, &src, buckets.as_deref()).await?
    };
    let (loads, plans) = build_plans(ctx, &db, &live, &live_buckets, tables, force).await?;

    if plans.is_empty() {
        ctx.say("nothing to load");
        return Ok(());
    }

    // A dry run's job is to say what would change, which a row-level diff
    // answers far better than a row count does.
    if dry_run && !ctx.json {
        let lock = Lock::read(&ctx.cfg.lock_path())?;
        let diffs = data_diff(ctx, &db, &live, &lock, None).await?;
        print!("{}", crate::diff::render(&diffs, false));
    }

    let reasons = load::confirmation_reasons(&src, &plans);
    if !reasons.is_empty() && !dry_run {
        ctx.say(load::render_plan(&db.source_name, &plans));
        for reason in &reasons {
            if !ctx.confirm(&format!("{reason}. Accept?"))? {
                ctx.say("aborted; nothing was changed");
                return Ok(());
            }
        }
    }

    let order = compute_order(&live, &ctx.cfg)?;
    let cycles = order::topological(&live, &order).cycles;
    let use_transaction = !no_transaction;

    if !db.engine().is_sql() {
        // Collections the target has not got yet are described only by the
        // lock, so the load reads them from there.
        let lock = Lock::read(&ctx.cfg.lock_path())?;
        let mut schema = lock.to_schema_for(&live);
        for (id, table) in &live.tables {
            schema.tables.insert(id.clone(), table.clone());
        }
        return load_documents(ctx, &db, &schema, &loads, dry_run, use_transaction).await;
    }

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
        match db.dialect().defer_constraints(all_deferrable) {
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
        match load::load_table(
            &mut conn,
            db.dialect(),
            table,
            &columns,
            rows,
            cfg,
            ctx.cfg.load.batch,
        )
        .await
        {
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

    if restored && let Some(stmt) = db.dialect().restore_constraints() {
        conn.execute(stmt).await?;
    }

    // Sequence fixup belongs inside the transaction wherever the engine allows
    // it, so a rollback undoes it too.
    let deferred_fixups = db.dialect().fixup_after_commit();
    if ctx.cfg.load.fix_sequences && !deferred_fixups {
        for (cfg, _) in &loads {
            let id = live.resolve(&cfg.id).unwrap_or_else(|| cfg.id.clone());
            let Some(table) = live.get(&id) else { continue };
            for stmt in db.dialect().sequence_fixups(table) {
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
    // which implicitly commits, so it can only run once the load itself has
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
        bail!("{prompt} (no terminal to ask on; pass --yes to accept)");
    }
    // Prompt on stderr so it never contaminates piped output.
    eprint!("{prompt} [y/N] ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn confirm_drift(prompt: &str, scope: &str) -> Result<Answer> {
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        bail!("{prompt} (no terminal to ask on; pass --yes to accept)");
    }
    eprintln!("{prompt}");
    eprint!("  [y] accept once  [n] refuse  [a] accept and remember for {scope} ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(match answer.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Answer::Once,
        "a" | "always" => Answer::Always,
        _ => Answer::No,
    })
}

// ---------------------------------------------------------------------------
// verify
// ---------------------------------------------------------------------------

pub fn cmd_verify(ctx: &Ctx) -> Result<()> {
    let lock = Lock::read(&ctx.cfg.lock_path())?;
    let out_dir = ctx.cfg.out_dir();
    // verify needs no database, so bare ids stand for themselves.
    let schema = lock.to_schema_in("");
    let selected = ctx.cfg.resolved_tables()?;

    let mut problems: Vec<String> = Vec::new();
    let mut checked = 0;

    for cfg in &selected {
        let Some(id) = schema.resolve(&cfg.id) else {
            problems.push(format!("{}: not in graine.lock", cfg.id));
            continue;
        };
        let Some(entry) = lock
            .files
            .get(&crate::lock::relative(&id, &schema.default_schema))
        else {
            problems.push(format!(
                "{id}: no file recorded in graine.lock (run `graine export`)"
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

        match format::read(&out_dir, &columns, cfg, &schema.default_schema, lock.engine) {
            Err(e) => problems.push(format!("{id}: {e:#}")),
            Ok(rows) => {
                if rows.len() as u64 != entry.rows {
                    problems.push(format!(
                        "{id}: {} rows on disk, {} recorded in graine.lock",
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
    for (name, entry) in &lock.bucket_files {
        let dir = out_dir.join("buckets").join(name);
        match BucketExport::read(&dir) {
            Err(e) => problems.push(format!("bucket {name}: {e:#}")),
            Ok(export) => {
                match export.hash() {
                    Ok(h) if h == entry.sha256 => {}
                    Ok(h) => problems.push(format!(
                        "bucket {name}: manifest hash {h} does not match graine.lock ({})",
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
            "seed files do not match graine.lock:\n{}",
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
            if lock.bucket_files.is_empty() {
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

// ---------------------------------------------------------------------------

pub async fn dispatch(cli: Cli) -> Result<()> {
    // These run before a config exists, so they do not build a Ctx.
    match &cli.command {
        Command::Init { url, force } => return cmd_init(&cli, url.clone(), *force).await,
        Command::Completions { shell } => {
            cmd_completions(*shell);
            return Ok(());
        }
        _ => {}
    }

    let ctx = Ctx::new(&cli)?;
    match cli.command {
        Command::Init { .. } | Command::Completions { .. } => unreachable!("handled above"),
        Command::Add {
            tables,
            no_parents,
            with_children,
            depth,
        } => cmd_add(&ctx, tables, no_parents, with_children, depth).await,
        Command::Status => cmd_status(&ctx).await,
        Command::Sources { no_connect } => cmd_sources(&ctx, no_connect).await,
        Command::Lock { check } => cmd_lock(&ctx, check).await,
        Command::Diff {
            data,
            full,
            tables,
            forget,
            forget_all,
        } => cmd_diff(&ctx, data, full, tables, forget, forget_all).await,
        Command::Export {
            tables,
            buckets,
            no_buckets,
            format,
            out,
            force,
            no_fk_check,
            follow_parents,
        } => {
            cmd_export(
                &ctx,
                tables,
                buckets,
                no_buckets,
                format,
                out,
                force,
                no_fk_check,
                follow_parents,
            )
            .await
        }
        Command::Plan { tables, tree } => cmd_plan(&ctx, tables, tree).await,
        Command::Load {
            tables,
            buckets,
            no_buckets,
            dry_run,
            no_transaction,
            force,
        } => {
            cmd_load(
                &ctx,
                tables,
                buckets,
                no_buckets,
                dry_run,
                no_transaction,
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
    use crate::lock::drift::{Drift, Report, Severity, Target};

    #[test]
    fn drift_json_reports_both_counts_and_every_item() {
        let report = Report {
            drifts: vec![
                Drift {
                    severity: Severity::Breaking,
                    target: Target::Column(TableId::new("public", "users"), "age".into()),
                    what: "int32 -> int16".into(),
                    note: "narrowing".into(),
                },
                Drift {
                    severity: Severity::Benign,
                    target: Target::Table(TableId::new("public", "users")),
                    what: "table added".into(),
                    note: String::new(),
                },
                Drift {
                    severity: Severity::Breaking,
                    target: Target::Bucket("project_files".into()),
                    what: "bucket does not exist".into(),
                    note: "run your migrations".into(),
                },
            ],
        };
        let parsed: serde_json::Value = serde_json::from_str(&drift_json(&report)).unwrap();
        assert_eq!(parsed["breaking"], 2);
        assert_eq!(parsed["benign"], 1);
        assert_eq!(parsed["drifts"].as_array().unwrap().len(), 3);
        assert_eq!(parsed["drifts"][0]["severity"], "breaking");
        assert_eq!(parsed["drifts"][0]["column"], "age");
        assert_eq!(parsed["drifts"][0]["table"], "public.users");
        assert_eq!(parsed["drifts"][0]["bucket"], serde_json::Value::Null);

        // A bucket drift carries a bucket name and no table.
        let bucket = parsed["drifts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["bucket"] == "project_files")
            .expect("the bucket drift should be present");
        assert_eq!(bucket["table"], serde_json::Value::Null);
        assert_eq!(bucket["target"], "bucket project_files");
    }
}

//! Commands that set a project up rather than move data.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::cli::Cli;
use crate::commands::{Ctx, introspect};
use crate::config::Engine;
use crate::db::{self, Db};
use crate::lock::{Lock, drift};
use crate::schema::TableId;
use crate::source::ResolvedSource;

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
        engine = Engine::from_url(url);
        let src = ResolvedSource {
            name: "init".into(),
            engine,
            url: url.clone(),
            read_only: true,
            origin: "--url".into(),
            storage: None,
        };
        let db = Db::connect(&src).await?;
        tables = db::list_tables(&db).await?;
        default_schema = db::default_schema(&db).await.unwrap_or(default_schema);
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
        println!("Add your tables under `tables:`, then run `graine lock`.");
    } else {
        println!(
            "Listed {} table{}. Trim the list, then run `graine lock`.",
            tables.len(),
            if tables.len() == 1 { "" } else { "s" }
        );
    }
    Ok(())
}

/// Append tables to the config, with the parents they need to load.
pub async fn cmd_add(ctx: &Ctx, wanted: Vec<String>, no_parents: bool) -> Result<()> {
    let (_src, db) = ctx.connect(1).await?;
    let full = db::introspect(&db).await?;

    let mut resolved = Vec::new();
    for name in &wanted {
        let id: TableId = name.parse().map_err(|e| anyhow::anyhow!("{name:?}: {e}"))?;
        match full.resolve(&id) {
            Some(r) => resolved.push(r),
            None => bail!(
                "{}",
                db::Missing {
                    wanted: vec![id],
                    found: full.tables.keys().cloned().collect(),
                }
                .describe()
            ),
        }
    }

    // Walk the foreign keys so the added slice can actually load.
    let mut needed: Vec<TableId> = Vec::new();
    let mut queue = resolved.clone();
    while let Some(id) = queue.pop() {
        if needed.contains(&id) {
            continue;
        }
        needed.push(id.clone());
        if no_parents {
            continue;
        }
        if let Some(table) = full.get(&id) {
            for fk in &table.foreign_keys {
                if let Some(parent) = full.resolve(&fk.references) {
                    queue.push(parent);
                }
            }
        }
    }
    needed.sort();

    let existing: Vec<TableId> = ctx
        .cfg
        .resolved_tables()?
        .into_iter()
        .map(|t| t.id)
        .collect();
    let new: Vec<String> = needed
        .iter()
        .filter(|id| {
            !existing
                .iter()
                .any(|e| full.resolve(e).as_ref() == Some(id))
        })
        .map(|id| id.file_stem(&full.default_schema))
        .collect();

    if new.is_empty() {
        ctx.say("already in the config; nothing to add");
        return Ok(());
    }

    let path = ctx.cfg.base_dir.join(crate::config::CONFIG_FILENAME);
    let mut text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    if !text.ends_with('\n') {
        text.push('\n');
    }
    // An empty `tables: {}` is a flow mapping, so indented entries cannot
    // follow it; turn it into a block mapping first.
    for empty in ["tables: {}\n", "tables: {}\r\n"] {
        if let Some(at) = text.find(empty) {
            text.replace_range(at..at + empty.len(), "tables:\n");
            break;
        }
    }
    if !text.contains("\ntables:") && !text.starts_with("tables:") {
        text.push_str("\ntables:\n");
    }
    for name in &new {
        text.push_str(&format!("  {name}: {{}}\n"));
    }
    crate::io::write_atomic(&path, text.as_bytes())?;

    let pulled: Vec<&String> = new.iter().filter(|n| !wanted.contains(n)).collect();
    ctx.say(format!("added {} to {}", new.join(", "), path.display()));
    if !pulled.is_empty() {
        ctx.say(format!(
            "{} came along as foreign-key parent{}",
            pulled
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            if pulled.len() == 1 { "" } else { "s" }
        ));
    }
    ctx.say("run `graine lock` to record the schema");
    Ok(())
}

/// One answer to "is my seed data current against this database".
pub async fn cmd_status(ctx: &Ctx) -> Result<()> {
    let lock_path = ctx.cfg.lock_path();
    let lock = Lock::read(&lock_path).ok();
    let (_src, db) = ctx.connect(1).await?;
    let live = introspect(&db, &ctx.cfg).await?;

    let report = match &lock {
        Some(l) => drift::classify(&l.to_schema_for(&live), &live),
        None => drift::Report::default(),
    };
    let (breaking, confirm, benign) = report.counts();

    // Row counts on both sides, so "current" means the data too, not just the
    // schema.
    let mut tables = Vec::new();
    for cfg in ctx.cfg.resolved_tables()? {
        let Some(id) = live.resolve(&cfg.id) else {
            continue;
        };
        let recorded = lock
            .as_ref()
            .and_then(|l| {
                l.files
                    .get(&crate::lock::relative(&id, &live.default_schema))
            })
            .map(|f| f.rows);
        let live_rows = db
            .query_text(&db.dialect().count_query(&id))
            .await
            .ok()
            .and_then(|r| r.first().and_then(|r| r.first().cloned().flatten()))
            .and_then(|v| v.trim().parse::<u64>().ok());
        tables.push((id, recorded, live_rows));
    }

    if ctx.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "locked": lock.is_some(),
                "fingerprint": lock.as_ref().map(|l| l.fingerprint.clone()),
                "drift": {"breaking": breaking, "needs_confirmation": confirm, "benign": benign},
                "tables": tables.iter().map(|(id, seeded, live_rows)| serde_json::json!({
                    "table": id.to_string(),
                    "seeded_rows": seeded,
                    "live_rows": live_rows,
                })).collect::<Vec<_>>(),
            }))?
        );
        return Ok(());
    }

    match &lock {
        None => ctx.say(format!(
            "no lock at {}; run `graine lock`",
            lock_path.display()
        )),
        Some(l) => ctx.say(format!("lock {} ({})", l.fingerprint, lock_path.display())),
    }
    ctx.say(if report.is_empty() {
        "schema: matches".to_string()
    } else {
        format!("schema: {breaking} breaking, {confirm} needing confirmation, {benign} benign")
    });

    let width = tables
        .iter()
        .map(|(id, ..)| id.to_string().chars().count())
        .max()
        .unwrap_or(0);
    for (id, seeded, live_rows) in &tables {
        let note = match (seeded, live_rows) {
            (Some(s), Some(l)) if s == l => "in sync".to_string(),
            (Some(s), Some(l)) => format!("seeded {s}, live {l}"),
            (None, Some(l)) => format!("not exported, live {l}"),
            (Some(s), None) => format!("seeded {s}, live unknown"),
            (None, None) => "not exported".to_string(),
        };
        ctx.say(format!(
            "  {:width$}  {note}",
            id.to_string(),
            width = width
        ));
    }
    Ok(())
}

/// Print a completion script for `shell`.
pub fn cmd_completions(shell: clap_complete::Shell) {
    use clap::CommandFactory;
    clap_complete::generate(shell, &mut Cli::command(), "graine", &mut std::io::stdout());
}

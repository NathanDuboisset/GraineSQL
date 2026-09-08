//! Commands that set a project up rather than move data.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::cli::Cli;
use crate::commands::{Ctx, introspect};
use crate::config::Engine;
use crate::db::{self, Db};
use crate::lock::{Lock, drift};
use crate::schema::{Schema, TableId};
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

/// Why a table ended up in the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Provenance {
    Named,
    Child(usize),
    Parent,
}

/// Parent -> the tables whose foreign keys point at it.
fn child_index(schema: &Schema) -> BTreeMap<TableId, Vec<TableId>> {
    let mut out: BTreeMap<TableId, Vec<TableId>> = BTreeMap::new();
    for (id, table) in &schema.tables {
        for fk in &table.foreign_keys {
            if let Some(parent) = schema.resolve(&fk.references)
                && parent != *id
            {
                out.entry(parent).or_default().push(id.clone());
            }
        }
    }
    for v in out.values_mut() {
        v.sort();
        v.dedup();
    }
    out
}

/// Append tables to the config, with the relatives they need to load.
pub async fn cmd_add(
    ctx: &Ctx,
    wanted: Vec<String>,
    no_parents: bool,
    with_children: bool,
    depth: Option<usize>,
) -> Result<()> {
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

    let mut how: BTreeMap<TableId, Provenance> = resolved
        .iter()
        .map(|id| (id.clone(), Provenance::Named))
        .collect();

    // Children first, then parents over the result: a child usually references
    // other tables too, and only the second pass brings those along.
    if with_children {
        let children = child_index(&full);
        let mut frontier = resolved.clone();
        for level in 1..=depth.unwrap_or(usize::MAX) {
            let mut next = Vec::new();
            for id in &frontier {
                for child in children.get(id).into_iter().flatten() {
                    if let std::collections::btree_map::Entry::Vacant(e) = how.entry(child.clone())
                    {
                        e.insert(Provenance::Child(level));
                        next.push(child.clone());
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }
    }

    if !no_parents {
        let mut queue: Vec<TableId> = how.keys().cloned().collect();
        while let Some(id) = queue.pop() {
            let Some(table) = full.get(&id) else { continue };
            for fk in &table.foreign_keys {
                let Some(parent) = full.resolve(&fk.references) else {
                    continue;
                };
                if let std::collections::btree_map::Entry::Vacant(e) = how.entry(parent.clone()) {
                    e.insert(Provenance::Parent);
                    queue.push(parent);
                }
            }
        }
    }

    let needed: Vec<TableId> = how.keys().cloned().collect();

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

    // Downward is the dangerous direction: in most schemas a couple of hops off
    // a central table reaches nearly everything, which is the failure `add`
    // exists to prevent. Show the list and ask before writing it.
    let children = new
        .iter()
        .filter(|n| matches!(by_stem(&how, &full, n), Some(Provenance::Child(_))))
        .count();
    if with_children && (children > 10 || children > 3 * wanted.len()) {
        ctx.say(render_provenance(&how, &full, &new));
        if !ctx.confirm(&format!("this adds {} tables. Continue?", new.len()))? {
            ctx.say("nothing was added");
            return Ok(());
        }
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

    if ctx.json {
        let items: Vec<serde_json::Value> = new
            .iter()
            .map(|n| match by_stem(&how, &full, n) {
                Some(Provenance::Child(d)) => {
                    serde_json::json!({"table": n, "via": "child", "depth": d})
                }
                Some(Provenance::Parent) => serde_json::json!({"table": n, "via": "parent"}),
                _ => serde_json::json!({"table": n, "via": "named"}),
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "path": path.display().to_string(),
                "added": items,
            }))?
        );
        return Ok(());
    }

    ctx.say(format!("added {} tables to {}", new.len(), path.display()));
    ctx.say(render_provenance(&how, &full, &new));

    // `add` works at table granularity; the referential check works at row
    // granularity, so a pre-existing filter can still orphan the new rows.
    let filtered: Vec<String> = ctx
        .cfg
        .resolved_tables()?
        .into_iter()
        .filter(|t| t.filter.is_some() || t.limit.is_some())
        .filter(|t| {
            full.resolve(&t.id).is_some_and(|id| {
                child_index(&full).get(&id).is_some_and(|kids| {
                    kids.iter()
                        .any(|k| new.contains(&k.file_stem(&full.default_schema)))
                })
            })
        })
        .map(|t| t.config_key)
        .collect();
    if !filtered.is_empty() {
        ctx.say(format!(
            "note: {} {} a filter, and the new tables reference {}; \
             `graine export` will check that every reference resolves",
            filtered.join(", "),
            if filtered.len() == 1 { "has" } else { "have" },
            if filtered.len() == 1 { "it" } else { "them" }
        ));
    }

    ctx.say("run `graine lock` to record the schema");
    Ok(())
}

fn by_stem(how: &BTreeMap<TableId, Provenance>, schema: &Schema, stem: &str) -> Option<Provenance> {
    how.iter()
        .find(|(id, _)| id.file_stem(&schema.default_schema) == stem)
        .map(|(_, p)| *p)
}

fn render_provenance(
    how: &BTreeMap<TableId, Provenance>,
    schema: &Schema,
    new: &[String],
) -> String {
    let pick = |want: fn(&Provenance) -> bool| -> Vec<&str> {
        new.iter()
            .filter(|n| by_stem(how, schema, n).as_ref().is_some_and(want))
            .map(|n| n.as_str())
            .collect()
    };
    let named = pick(|p| matches!(p, Provenance::Named));
    let kids = pick(|p| matches!(p, Provenance::Child(_)));
    let parents = pick(|p| matches!(p, Provenance::Parent));

    let deepest = new
        .iter()
        .filter_map(|n| match by_stem(how, schema, n) {
            Some(Provenance::Child(d)) => Some(d),
            _ => None,
        })
        .max()
        .unwrap_or(0);

    let mut out = String::new();
    for (label, group, suffix) in [
        ("named", named, String::new()),
        ("children", kids, format!("  ({deepest} level deep)")),
        ("parents", parents, "  (needed to load the above)".into()),
    ] {
        if !group.is_empty() {
            out.push_str(&format!("  {label:<9} {}{suffix}\n", group.join(", ")));
        }
    }
    out.trim_end().to_string()
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

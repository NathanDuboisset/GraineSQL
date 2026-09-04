//! Pulling in the parent rows a filtered slice needs to be loadable.
//!
//! `export::check_referential_closure` reports that a slice is broken. This
//! repairs it, by walking the foreign-key graph over key *values* until every
//! reference resolves.
//!
//! The walk runs before any file is written, reading only key columns, so the
//! export itself stays a single streaming pass and the pulled rows arrive under
//! the same `ORDER BY` as the rest.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};

use crate::config::ResolvedTable;
use crate::db::Db;
use crate::export;
use crate::schema::{Column, Schema, TableId};

/// Key tuples to pull into each table, beyond what its own filter selects.
pub type Pulled = BTreeMap<TableId, BTreeSet<Vec<String>>>;

/// How many times the walk may find new parents before giving up.
pub const DEFAULT_MAX_DEPTH: usize = 10;

#[derive(Debug, Clone)]
pub struct Pull {
    pub table: TableId,
    pub rows: usize,
}

/// Walk the foreign-key graph until every reference in the slice resolves.
pub async fn resolve(
    db: &Db,
    live: &Schema,
    cfgs: &[ResolvedTable],
    max_depth: usize,
) -> Result<(Pulled, Vec<Pull>)> {
    let ids: Vec<TableId> = cfgs.iter().filter_map(|c| live.resolve(&c.id)).collect();
    let to_index = export::columns_to_index(live, &ids);

    let mut pulled: Pulled = BTreeMap::new();
    for round in 0..=max_depth {
        let mut keys: BTreeMap<TableId, export::KeyIndex> = BTreeMap::new();
        for cfg in cfgs {
            let Some(id) = live.resolve(&cfg.id) else {
                continue;
            };
            let sets = to_index.get(&id).cloned().unwrap_or_default();
            if sets.is_empty() {
                continue;
            }
            keys.insert(
                id.clone(),
                read_keys(db, live, &id, cfg, &sets, pulled.get(&id)).await?,
            );
        }

        let missing = dangling_values(live, &keys);
        if missing.is_empty() {
            break;
        }
        if round == max_depth {
            let names: Vec<String> = missing.keys().map(|t| t.to_string()).collect();
            bail!(
                "following foreign keys did not settle after {max_depth} rounds; still \
                 pulling rows into {}.\n\
                 A cycle of filters that exclude each other's rows can do this. Narrow \
                 the filters, or pass --no-fk-check to export the slice as it is.",
                names.join(", ")
            );
        }

        let mut grew = false;
        for (parent, values) in missing {
            let entry = pulled.entry(parent).or_default();
            for v in values {
                grew |= entry.insert(v);
            }
        }
        if !grew {
            break;
        }
    }

    let summary = pulled
        .iter()
        .map(|(table, rows)| Pull {
            table: table.clone(),
            rows: rows.len(),
        })
        .collect();
    Ok((pulled, summary))
}

/// The key index for one table, reading only the columns the check needs.
async fn read_keys(
    db: &Db,
    live: &Schema,
    id: &TableId,
    cfg: &ResolvedTable,
    sets: &[Vec<String>],
    extra: Option<&BTreeSet<Vec<String>>>,
) -> Result<export::KeyIndex> {
    let table = live
        .get(id)
        .ok_or_else(|| anyhow::anyhow!("table {id} is not in the live schema"))?;
    let all = export::selected_columns(table, cfg)?;

    let mut wanted: Vec<String> = Vec::new();
    for set in sets {
        for c in set {
            if !wanted.contains(c) {
                wanted.push(c.clone());
            }
        }
    }
    let select: Vec<&Column> = wanted
        .iter()
        .filter_map(|n| all.iter().copied().find(|c| c.name == *n))
        .collect();
    if select.len() != wanted.len() {
        return Ok(BTreeMap::new());
    }

    let also = extra.and_then(|tuples| {
        let pk: Vec<&Column> = table
            .primary_key
            .iter()
            .filter_map(|k| table.column(k))
            .collect();
        let list: Vec<Vec<String>> = tuples.iter().cloned().collect();
        db.dialect().tuple_predicate(&pk, &list)
    });

    let sql = export::build_query_with(db.dialect(), table, &all, &select, cfg, also.as_deref())?;

    let positions: Vec<Vec<usize>> = sets
        .iter()
        .map(|set| {
            set.iter()
                .filter_map(|c| select.iter().position(|s| s.name == *c))
                .collect()
        })
        .collect();

    let mut index: export::KeyIndex = BTreeMap::new();
    for set in sets {
        index.entry(set.clone()).or_default();
    }
    db.for_each_text_row(&sql, |row| {
        for (set, pos) in sets.iter().zip(&positions) {
            if pos.len() != set.len() {
                continue;
            }
            let tuple: Option<Vec<String>> =
                pos.iter().map(|p| row.get(*p).cloned().flatten()).collect();
            // A null anywhere means the reference is absent, not dangling.
            if let Some(tuple) = tuple {
                index.entry(set.clone()).or_default().insert(tuple);
            }
        }
        Ok(())
    })
    .await
    .with_context(|| format!("reading keys of {id}"))?;

    Ok(index)
}

/// Which parent key tuples the slice references but does not contain.
fn dangling_values(
    live: &Schema,
    keys: &BTreeMap<TableId, export::KeyIndex>,
) -> BTreeMap<TableId, BTreeSet<Vec<String>>> {
    let mut out: BTreeMap<TableId, BTreeSet<Vec<String>>> = BTreeMap::new();

    for (id, index) in keys {
        let Some(table) = live.get(id) else { continue };
        for fk in &table.foreign_keys {
            let Some(child) = index.get(&fk.columns) else {
                continue;
            };
            if child.is_empty() {
                continue;
            }
            let parent_id = live
                .resolve(&fk.references)
                .unwrap_or(fk.references.clone());
            let parent = if parent_id == *id {
                index.get(&fk.ref_columns)
            } else {
                keys.get(&parent_id).and_then(|k| k.get(&fk.ref_columns))
            };

            let missing: BTreeSet<Vec<String>> = match parent {
                None => child.clone(),
                Some(have) => child.difference(have).cloned().collect(),
            };
            if !missing.is_empty() {
                out.entry(parent_id).or_default().extend(missing);
            }
        }
    }
    out
}

/// One line per table that gained rows.
pub fn render(pulls: &[Pull]) -> String {
    pulls
        .iter()
        .filter(|p| p.rows > 0)
        .map(|p| {
            format!(
                "  {}: {} row{} pulled in to satisfy a foreign key\n",
                p.table,
                p.rows,
                if p.rows == 1 { "" } else { "s" }
            )
        })
        .collect()
}

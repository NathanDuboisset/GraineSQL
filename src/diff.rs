//! What a load would actually change, row by row.
//!
//! Read-only and run before the transaction opens. `status` answers "are the
//! counts the same"; this answers "which rows differ, and in what".

use anyhow::{Context, Result};
use std::collections::BTreeMap;

use crate::config::{LoadMode, ResolvedTable};
use crate::db::Db;
use crate::dialect::Dialect;
use crate::schema::{Column, Table};
use crate::value::Value;

/// How many key values to name before summarising the rest.
const SHOWN: usize = 10;

/// Key values per lookup query, kept under the engine's bind-parameter ceiling
/// even though these are literals rather than binds.
pub fn chunk_for(engine: crate::config::Engine) -> usize {
    match engine.dialect() {
        crate::config::Engine::Sqlite => 500,
        _ => 5_000,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Added,
    /// `(column, before, after)` for each column whose text form moved.
    Updated(Vec<(String, String, String)>),
    Unchanged,
}

#[derive(Debug, Clone)]
pub struct RowDiff {
    pub key: Vec<String>,
    pub change: Change,
}

#[derive(Debug, Clone)]
pub struct TableDiff {
    pub table: crate::schema::TableId,
    pub rows: Vec<RowDiff>,
    /// Live keys the seed files do not have. Only populated for
    /// `truncate_first`, which is the only mode that removes anything.
    pub removed: Vec<Vec<String>>,
    /// Set when the table has no usable key, so rows cannot be matched up.
    pub unkeyed: bool,
}

impl TableDiff {
    pub fn counts(&self) -> (usize, usize, usize) {
        let mut added = 0;
        let mut updated = 0;
        let mut same = 0;
        for r in &self.rows {
            match r.change {
                Change::Added => added += 1,
                Change::Updated(_) => updated += 1,
                Change::Unchanged => same += 1,
            }
        }
        (added, updated, same)
    }

    pub fn is_noop(&self) -> bool {
        let (a, u, _) = self.counts();
        a == 0 && u == 0 && self.removed.is_empty()
    }
}

/// Compare one table's seed rows against the live ones.
pub async fn table_diff(
    db: &Db,
    table: &Table,
    columns: &[&Column],
    rows: &[Vec<Value>],
    cfg: &ResolvedTable,
    batch: usize,
) -> Result<TableDiff> {
    let key = crate::load::upsert_key(table, cfg)?;
    let positions: Vec<usize> = key
        .iter()
        .filter_map(|k| columns.iter().position(|c| c.name == *k))
        .collect();

    if key.is_empty() || positions.len() != key.len() {
        return Ok(TableDiff {
            table: table.id.clone(),
            rows: Vec::new(),
            removed: Vec::new(),
            unkeyed: true,
        });
    }

    let seed: Vec<(Vec<String>, &Vec<Value>)> =
        rows.iter().map(|r| (key_of(r, &positions), r)).collect();

    let live = fetch_live(db, table, columns, &key, &positions, &seed, batch).await?;

    let mut out = Vec::with_capacity(seed.len());
    for (k, row) in &seed {
        let change = match live.get(k) {
            None => Change::Added,
            Some(existing) => {
                let moved: Vec<(String, String, String)> = columns
                    .iter()
                    .zip(*row)
                    .zip(existing)
                    .filter(|((_, want), have)| text(want) != text(have))
                    .map(|((col, want), have)| (col.name.clone(), text(have), text(want)))
                    .collect();
                if moved.is_empty() {
                    Change::Unchanged
                } else {
                    Change::Updated(moved)
                }
            }
        };
        out.push(RowDiff {
            key: k.clone(),
            change,
        });
    }

    let removed = if cfg.load_mode == LoadMode::TruncateFirst {
        let seeded: std::collections::BTreeSet<&Vec<String>> =
            seed.iter().map(|(k, _)| k).collect();
        live_keys(db, table, &key)
            .await?
            .into_iter()
            .filter(|k| !seeded.contains(k))
            .collect()
    } else {
        Vec::new()
    };

    Ok(TableDiff {
        table: table.id.clone(),
        rows: out,
        removed,
        unkeyed: false,
    })
}

fn key_of(row: &[Value], positions: &[usize]) -> Vec<String> {
    positions.iter().map(|p| text(&row[*p])).collect()
}

/// The comparison form. Canonical text rather than `Value`, so engine
/// formatting does not read as a change: Postgres hands back `{"a": 1}` where
/// the seed file holds `{"a":1}`, and those are the same document.
fn text(v: &Value) -> String {
    match v {
        Value::Null => "\0NULL".into(),
        Value::Json(raw) => match serde_json::from_str::<serde_json::Value>(raw) {
            Ok(parsed) => {
                let mut out = String::new();
                crate::format::json::write_json(&mut out, &parsed, false, 0, true);
                out
            }
            Err(_) => raw.clone(),
        },
        _ => v.to_text().unwrap_or_default(),
    }
}

/// Fetch the live rows matching the seed keys, chunked to stay inside the
/// engine's bind-parameter limit.
async fn fetch_live(
    db: &Db,
    table: &Table,
    columns: &[&Column],
    key: &[String],
    positions: &[usize],
    seed: &[(Vec<String>, &Vec<Value>)],
    batch: usize,
) -> Result<BTreeMap<Vec<String>, Vec<Value>>> {
    let dialect = db.dialect();
    let select = columns
        .iter()
        .map(|c| dialect.read_expr(c))
        .collect::<Vec<_>>()
        .join(", ");
    let classes: Vec<_> = columns.iter().map(|c| c.class.clone()).collect();

    let mut found = BTreeMap::new();
    for chunk in seed.chunks(batch.max(1)) {
        let keys: Vec<&Vec<String>> = chunk.iter().map(|(k, _)| k).collect();
        let Some(predicate) = key_predicate(dialect, table, key, &keys) else {
            continue;
        };
        let sql = format!(
            "SELECT {select} FROM {} WHERE {predicate}",
            dialect.quote_table(&table.id)
        );
        for row in db
            .query_text(&sql)
            .await
            .with_context(|| format!("reading live rows of {}", table.id))?
        {
            let values: Vec<Value> = classes
                .iter()
                .zip(&row)
                .map(|(class, text)| Value::parse(class, text.as_deref()))
                .collect::<Result<_>>()?;
            found.insert(key_of(&values, positions), values);
        }
    }
    Ok(found)
}

/// Every live key, for spotting rows a `truncate_first` would remove.
async fn live_keys(db: &Db, table: &Table, key: &[String]) -> Result<Vec<Vec<String>>> {
    let dialect = db.dialect();
    let cols: Vec<&Column> = key.iter().filter_map(|k| table.column(k)).collect();
    if cols.len() != key.len() {
        return Ok(Vec::new());
    }
    let select = cols
        .iter()
        .map(|c| dialect.read_expr(c))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!("SELECT {select} FROM {}", dialect.quote_table(&table.id));
    let mut out = Vec::new();
    for row in db
        .query_text(&sql)
        .await
        .with_context(|| format!("reading live keys of {}", table.id))?
    {
        out.push(
            row.iter()
                .map(|v| v.clone().unwrap_or_else(|| "\0NULL".into()))
                .collect(),
        );
    }
    Ok(out)
}

/// `k IN ('a','b')` for a single-column key, else `(a = 'x' AND b = 'y') OR ...`.
///
/// Spelled out rather than using row-value `IN`, whose support differs across
/// the three engines.
fn key_predicate(
    dialect: &dyn Dialect,
    table: &Table,
    key: &[String],
    keys: &[&Vec<String>],
) -> Option<String> {
    if keys.is_empty() {
        return None;
    }
    let cols: Vec<&Column> = key.iter().filter_map(|k| table.column(k)).collect();
    if cols.len() != key.len() {
        return None;
    }

    if cols.len() == 1 {
        let list = keys
            .iter()
            .map(|k| quote(&k[0]))
            .collect::<Vec<_>>()
            .join(", ");
        return Some(format!("{} IN ({list})", dialect.read_expr(cols[0])));
    }

    Some(
        keys.iter()
            .map(|k| {
                let terms: Vec<String> = cols
                    .iter()
                    .zip(*k)
                    .map(|(c, v)| format!("{} = {}", dialect.read_expr(c), quote(v)))
                    .collect();
                format!("({})", terms.join(" AND "))
            })
            .collect::<Vec<_>>()
            .join(" OR "),
    )
}

/// The key is compared against `read_expr`, which is text on every engine, so
/// a text literal is always the right shape.
fn quote(v: &str) -> String {
    format!("'{}'", v.replace('\'', "''"))
}

/// Render the diffs the way `graine diff --data` prints them.
pub fn render(diffs: &[TableDiff], full: bool) -> String {
    let mut out = String::new();
    let width = diffs
        .iter()
        .map(|d| d.table.to_string().chars().count())
        .max()
        .unwrap_or(0)
        .min(40);

    for d in diffs {
        let name = d.table.to_string();
        if d.unkeyed {
            out.push_str(&format!(
                "{name:<width$}  no primary key or unique constraint, so rows cannot be matched\n"
            ));
            continue;
        }
        let (added, updated, same) = d.counts();
        let mut line = format!("{name:<width$}");
        for (n, sign) in [(added, '+'), (updated, '~'), (same, '=')] {
            if n > 0 {
                line.push_str(&format!("  {sign}{n}"));
            }
        }
        if !d.removed.is_empty() {
            line.push_str(&format!("  -{}", d.removed.len()));
        }
        if d.is_noop() {
            line.push_str("  (no change)");
        }
        out.push_str(&line);
        out.push('\n');

        let mut shown = 0;
        for r in &d.rows {
            if matches!(r.change, Change::Unchanged) {
                continue;
            }
            if !full && shown == SHOWN {
                break;
            }
            shown += 1;
            match &r.change {
                Change::Added => out.push_str(&format!("  + {}\n", r.key.join(", "))),
                Change::Updated(cols) => {
                    let detail: Vec<String> = cols
                        .iter()
                        .map(|(c, before, after)| format!("{c} {before} -> {after}"))
                        .collect();
                    out.push_str(&format!(
                        "  ~ {}  {}\n",
                        r.key.join(", "),
                        detail.join("; ")
                    ));
                }
                Change::Unchanged => {}
            }
        }
        for k in d.removed.iter().take(if full { usize::MAX } else { SHOWN }) {
            out.push_str(&format!("  - {}\n", k.join(", ")));
        }

        let (a, u, _) = d.counts();
        let listed = shown + d.removed.len().min(if full { usize::MAX } else { SHOWN });
        let total = a + u + d.removed.len();
        if !full && total > listed {
            out.push_str(&format!("  … and {} more\n", total - listed));
        }
    }
    out
}

pub fn to_json(diffs: &[TableDiff]) -> serde_json::Value {
    let tables: Vec<serde_json::Value> = diffs
        .iter()
        .map(|d| {
            let (added, updated, unchanged) = d.counts();
            serde_json::json!({
                "table": d.table.to_string(),
                "unkeyed": d.unkeyed,
                "added": added,
                "updated": updated,
                "unchanged": unchanged,
                "removed": d.removed.len(),
                "rows": d.rows.iter().filter(|r| !matches!(r.change, Change::Unchanged)).map(|r| {
                    match &r.change {
                        Change::Added => serde_json::json!({"key": r.key, "change": "added"}),
                        Change::Updated(cols) => serde_json::json!({
                            "key": r.key,
                            "change": "updated",
                            "columns": cols.iter().map(|(c, b, a)| serde_json::json!({
                                "column": c, "before": b, "after": a
                            })).collect::<Vec<_>>(),
                        }),
                        Change::Unchanged => serde_json::Value::Null,
                    }
                }).collect::<Vec<_>>(),
            })
        })
        .collect();
    serde_json::json!({ "tables": tables })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::Postgres;
    use crate::schema::{TableId, TypeClass};

    fn col(name: &str, class: TypeClass) -> Column {
        Column {
            name: name.into(),
            sql_type: "text".into(),
            class,
            nullable: true,
            has_default: false,
            generated: false,
            identity: false,
        }
    }

    fn table(cols: Vec<Column>, pk: Vec<&str>) -> Table {
        Table {
            id: TableId::new("public", "t"),
            columns: cols,
            primary_key: pk.into_iter().map(String::from).collect(),
            unique: vec![],
            foreign_keys: vec![],
        }
    }

    #[test]
    fn a_single_column_key_uses_an_in_list() {
        let t = table(vec![col("id", TypeClass::Int { bits: 32 })], vec!["id"]);
        let keys = [vec!["1".to_string()], vec!["2".to_string()]];
        let refs: Vec<&Vec<String>> = keys.iter().collect();
        let p = key_predicate(&Postgres, &t, &["id".into()], &refs).unwrap();
        assert_eq!(p, "\"id\"::text IN ('1', '2')");
    }

    #[test]
    fn a_composite_key_expands_to_or_of_ands() {
        let t = table(
            vec![
                col("a", TypeClass::Text { max_len: None }),
                col("b", TypeClass::Text { max_len: None }),
            ],
            vec!["a", "b"],
        );
        let keys = [vec!["x".to_string(), "y".to_string()]];
        let refs: Vec<&Vec<String>> = keys.iter().collect();
        let p = key_predicate(&Postgres, &t, &["a".into(), "b".into()], &refs).unwrap();
        assert_eq!(p, "(\"a\"::text = 'x' AND \"b\"::text = 'y')");
    }

    #[test]
    fn a_quote_in_a_key_cannot_break_out_of_the_literal() {
        let t = table(
            vec![col("id", TypeClass::Text { max_len: None })],
            vec!["id"],
        );
        let keys = [vec!["o'brien".to_string()]];
        let refs: Vec<&Vec<String>> = keys.iter().collect();
        let p = key_predicate(&Postgres, &t, &["id".into()], &refs).unwrap();
        assert_eq!(p, "\"id\"::text IN ('o''brien')");
    }

    #[test]
    fn counts_split_added_updated_and_unchanged() {
        let d = TableDiff {
            table: TableId::bare("t"),
            rows: vec![
                RowDiff {
                    key: vec!["1".into()],
                    change: Change::Added,
                },
                RowDiff {
                    key: vec!["2".into()],
                    change: Change::Updated(vec![("x".into(), "a".into(), "b".into())]),
                },
                RowDiff {
                    key: vec!["3".into()],
                    change: Change::Unchanged,
                },
            ],
            removed: vec![],
            unkeyed: false,
        };
        assert_eq!(d.counts(), (1, 1, 1));
        assert!(!d.is_noop());

        let rendered = render(std::slice::from_ref(&d), true);
        assert!(rendered.contains("+1"), "{rendered}");
        assert!(rendered.contains("~1"), "{rendered}");
        assert!(rendered.contains("x a -> b"), "{rendered}");
    }

    #[test]
    fn a_table_with_nothing_to_do_says_so() {
        let d = TableDiff {
            table: TableId::bare("t"),
            rows: vec![RowDiff {
                key: vec!["1".into()],
                change: Change::Unchanged,
            }],
            removed: vec![],
            unkeyed: false,
        };
        assert!(d.is_noop());
        assert!(render(&[d], false).contains("(no change)"));
    }
}

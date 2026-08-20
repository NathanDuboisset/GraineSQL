//! Deterministic topological ordering of tables by foreign-key dependency.

use std::collections::{BTreeMap, BTreeSet};

use crate::schema::{Schema, TableId};

/// A dependency cycle among the selected tables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cycle {
    /// Tables in the cycle, rotated to start at the smallest id so the report is
    /// stable across runs.
    pub tables: Vec<TableId>,
    /// True when every foreign key in the cycle is `DEFERRABLE`, which means the
    /// load can proceed inside one transaction with constraints deferred.
    pub all_deferrable: bool,
}

/// The result of ordering: a load sequence plus any cycles found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Order {
    /// Insert order. Parents precede children. Tables in a cycle appear in
    /// sorted order at the point the cycle was broken.
    pub tables: Vec<TableId>,
    pub cycles: Vec<Cycle>,
}

impl Order {
    /// Delete/truncate order, the exact reverse of the insert order.
    pub fn reversed(&self) -> Vec<TableId> {
        self.tables.iter().rev().cloned().collect()
    }
}

/// Order `selected` so that a table's foreign-key parents come first.
///
/// Only dependencies *within* `selected` constrain the order; a foreign key
/// pointing at an unselected table is ignored, since we are not loading it.
///
/// Ties are broken by table id, so the output depends only on the schema and the
/// selection, never on hash iteration order or on the order tables were listed.
pub fn topological(schema: &Schema, selected: &[TableId]) -> Order {
    // Canonicalise and dedupe up front so an unqualified id in the config lines
    // up with the qualified id introspection produced.
    let nodes: BTreeSet<TableId> = selected
        .iter()
        .map(|id| schema.resolve(id).unwrap_or_else(|| id.clone()))
        .collect();

    // parents[child] = set of tables that must be loaded before it.
    let mut parents: BTreeMap<&TableId, BTreeSet<TableId>> =
        nodes.iter().map(|n| (n, BTreeSet::new())).collect();
    // children[parent] = tables waiting on it.
    let mut children: BTreeMap<&TableId, BTreeSet<TableId>> =
        nodes.iter().map(|n| (n, BTreeSet::new())).collect();
    let mut deferrable: BTreeMap<(TableId, TableId), bool> = BTreeMap::new();

    for node in &nodes {
        let Some(table) = schema.get(node) else {
            continue;
        };
        for fk in &table.foreign_keys {
            let Some(parent) = schema.resolve(&fk.references) else {
                continue;
            };
            if !nodes.contains(&parent) || parent == *node {
                // Unselected parent, or a self-reference, a self-reference is
                // satisfied within a single table's insert batch, so it never
                // constrains table ordering.
                continue;
            }
            parents
                .get_mut(node)
                .expect("node was seeded")
                .insert(parent.clone());
            children
                .get_mut(&parent)
                .expect("parent is in nodes")
                .insert(node.clone());
            // If any FK between the pair is non-deferrable, the pair is not.
            deferrable
                .entry((parent.clone(), node.clone()))
                .and_modify(|d| *d &= fk.deferrable)
                .or_insert(fk.deferrable);
        }
    }

    // Kahn's algorithm over a BTreeSet frontier, which pops the smallest id
    // first and makes the whole traversal deterministic.
    let mut remaining: BTreeMap<TableId, usize> = parents
        .iter()
        .map(|(id, ps)| ((*id).clone(), ps.len()))
        .collect();
    let mut ready: BTreeSet<TableId> = remaining
        .iter()
        .filter(|(_, n)| **n == 0)
        .map(|(id, _)| id.clone())
        .collect();
    for id in &ready {
        remaining.remove(id);
    }

    let mut ordered = Vec::with_capacity(nodes.len());
    while let Some(next) = ready.iter().next().cloned() {
        ready.remove(&next);
        ordered.push(next.clone());
        for child in children.get(&next).into_iter().flatten() {
            if let Some(count) = remaining.get_mut(child) {
                *count -= 1;
                if *count == 0 {
                    remaining.remove(child);
                    ready.insert(child.clone());
                }
            }
        }
    }

    // Anything left is in or downstream of a cycle.
    let mut cycles = Vec::new();
    if !remaining.is_empty() {
        let stuck: BTreeSet<TableId> = remaining.keys().cloned().collect();
        cycles = find_cycles(&stuck, &parents, &deferrable);
        // Append the stuck tables in sorted order so the load is still fully
        // specified; the caller decides whether deferring makes it legal.
        ordered.extend(stuck);
    }

    Order {
        tables: ordered,
        cycles,
    }
}

/// Enumerate the strongly connected components among the stuck tables that
/// actually form cycles, so the error message can name them.
fn find_cycles(
    stuck: &BTreeSet<TableId>,
    parents: &BTreeMap<&TableId, BTreeSet<TableId>>,
    deferrable: &BTreeMap<(TableId, TableId), bool>,
) -> Vec<Cycle> {
    let mut cycles = Vec::new();
    let mut visited: BTreeSet<TableId> = BTreeSet::new();

    for start in stuck {
        if visited.contains(start) {
            continue;
        }
        // Walk parent edges from `start`, staying inside `stuck`, until we
        // revisit a node on the current path, that closes a cycle.
        let mut path: Vec<TableId> = Vec::new();
        let mut on_path: BTreeSet<TableId> = BTreeSet::new();
        let mut cursor = start.clone();
        loop {
            if on_path.contains(&cursor) {
                let at = path.iter().position(|p| *p == cursor).expect("on_path");
                let mut members: Vec<TableId> = path[at..].to_vec();
                // Rotate to start at the smallest id for a stable report.
                if let Some(min_at) = min_index(&members) {
                    members.rotate_left(min_at);
                }
                let all_deferrable = cycle_edges(&members)
                    .all(|(a, b)| deferrable.get(&(a, b)).copied().unwrap_or(false));
                visited.extend(members.iter().cloned());
                cycles.push(Cycle {
                    tables: members,
                    all_deferrable,
                });
                break;
            }
            if visited.contains(&cursor) {
                break;
            }
            on_path.insert(cursor.clone());
            path.push(cursor.clone());
            let next = parents
                .get(&cursor)
                .into_iter()
                .flatten()
                .find(|p| stuck.contains(*p))
                .cloned();
            match next {
                Some(n) => cursor = n,
                None => {
                    visited.extend(path);
                    break;
                }
            }
        }
    }

    cycles.sort_by(|a, b| a.tables.cmp(&b.tables));
    cycles
}

/// Directed edges of a cycle as (parent, child) pairs.
///
/// `members` is a parent-walk, so `members[i]`'s parent is `members[i + 1]`,
/// wrapping around at the end.
fn cycle_edges(members: &[TableId]) -> impl Iterator<Item = (TableId, TableId)> + '_ {
    (0..members.len()).map(move |i| {
        let child = members[i].clone();
        let parent = members[(i + 1) % members.len()].clone();
        (parent, child)
    })
}

fn min_index(items: &[TableId]) -> Option<usize> {
    items
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| a.cmp(b))
        .map(|(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{ForeignKey, Table};
    use indexmap::IndexMap;

    /// Build a schema from `(table, [(fk_target, deferrable)])` pairs.
    fn schema(specs: &[(&str, &[(&str, bool)])]) -> Schema {
        let mut tables = IndexMap::new();
        for (name, fks) in specs {
            let id = TableId::new("public", *name);
            let foreign_keys = fks
                .iter()
                .enumerate()
                .map(|(i, (target, deferrable))| ForeignKey {
                    name: format!("{name}_fk{i}"),
                    columns: vec![format!("{target}_id")],
                    references: TableId::new("public", *target),
                    ref_columns: vec!["id".to_string()],
                    deferrable: *deferrable,
                })
                .collect();
            tables.insert(
                id.clone(),
                Table {
                    id,
                    columns: vec![],
                    primary_key: vec!["id".into()],
                    unique: vec![],
                    foreign_keys,
                },
            );
        }
        Schema {
            default_schema: "public".into(),
            tables,
            enums: IndexMap::new(),
        }
    }

    fn ids(names: &[&str]) -> Vec<TableId> {
        names.iter().map(|n| TableId::new("public", *n)).collect()
    }

    fn names(ids: &[TableId]) -> Vec<String> {
        ids.iter().map(|i| i.name.clone()).collect()
    }

    #[test]
    fn parents_come_before_children() {
        let s = schema(&[
            ("orders", &[("users", false)]),
            ("users", &[("orgs", false)]),
            ("orgs", &[]),
        ]);
        let o = topological(&s, &ids(&["orders", "users", "orgs"]));
        assert!(o.cycles.is_empty());
        assert_eq!(names(&o.tables), ["orgs", "users", "orders"]);
    }

    #[test]
    fn order_is_independent_of_input_order() {
        let s = schema(&[
            ("orders", &[("users", false)]),
            ("users", &[("orgs", false)]),
            ("orgs", &[]),
        ]);
        let a = topological(&s, &ids(&["orders", "users", "orgs"]));
        let b = topological(&s, &ids(&["orgs", "orders", "users"]));
        let c = topological(&s, &ids(&["users", "orgs", "orders"]));
        assert_eq!(a, b);
        assert_eq!(b, c);
    }

    #[test]
    fn independent_tables_are_ordered_by_name_not_by_chance() {
        let s = schema(&[("zebra", &[]), ("alpha", &[]), ("middle", &[])]);
        let o = topological(&s, &ids(&["zebra", "alpha", "middle"]));
        assert_eq!(names(&o.tables), ["alpha", "middle", "zebra"]);
    }

    #[test]
    fn reverse_order_is_the_delete_order() {
        let s = schema(&[("orders", &[("users", false)]), ("users", &[])]);
        let o = topological(&s, &ids(&["orders", "users"]));
        assert_eq!(names(&o.tables), ["users", "orders"]);
        assert_eq!(names(&o.reversed()), ["orders", "users"]);
    }

    #[test]
    fn foreign_keys_to_unselected_tables_do_not_constrain() {
        // `users` references `orgs`, but only `users` is being loaded.
        let s = schema(&[("users", &[("orgs", false)]), ("orgs", &[])]);
        let o = topological(&s, &ids(&["users"]));
        assert!(o.cycles.is_empty());
        assert_eq!(names(&o.tables), ["users"]);
    }

    #[test]
    fn self_reference_does_not_create_a_cycle() {
        // A manager_id pointing at the same table is resolved within the table's
        // own insert batch, so it must not be reported as unloadable.
        let s = schema(&[("employees", &[("employees", false)])]);
        let o = topological(&s, &ids(&["employees"]));
        assert!(
            o.cycles.is_empty(),
            "self-reference is not a cycle: {:?}",
            o.cycles
        );
        assert_eq!(names(&o.tables), ["employees"]);
    }

    #[test]
    fn two_table_cycle_is_detected_and_named() {
        let s = schema(&[("users", &[("orgs", false)]), ("orgs", &[("users", false)])]);
        let o = topological(&s, &ids(&["users", "orgs"]));
        assert_eq!(o.cycles.len(), 1);
        let cycle = &o.cycles[0];
        assert_eq!(cycle.tables.len(), 2);
        assert!(!cycle.all_deferrable);
        // Every table still appears in the output so the caller can report fully.
        assert_eq!(o.tables.len(), 2);
    }

    #[test]
    fn deferrable_cycle_is_flagged_as_loadable() {
        let s = schema(&[("users", &[("orgs", true)]), ("orgs", &[("users", true)])]);
        let o = topological(&s, &ids(&["users", "orgs"]));
        assert_eq!(o.cycles.len(), 1);
        assert!(
            o.cycles[0].all_deferrable,
            "an all-DEFERRABLE cycle can be loaded with constraints deferred"
        );
    }

    #[test]
    fn a_single_non_deferrable_edge_poisons_the_cycle() {
        let s = schema(&[("users", &[("orgs", true)]), ("orgs", &[("users", false)])]);
        let o = topological(&s, &ids(&["users", "orgs"]));
        assert_eq!(o.cycles.len(), 1);
        assert!(!o.cycles[0].all_deferrable);
    }

    #[test]
    fn cycle_report_starts_at_the_smallest_id_for_stability() {
        let s = schema(&[
            ("beta", &[("gamma", false)]),
            ("gamma", &[("alpha", false)]),
            ("alpha", &[("beta", false)]),
        ]);
        let a = topological(&s, &ids(&["beta", "gamma", "alpha"]));
        let b = topological(&s, &ids(&["gamma", "alpha", "beta"]));
        assert_eq!(a.cycles, b.cycles);
        assert_eq!(a.cycles[0].tables[0].name, "alpha");
    }

    #[test]
    fn acyclic_tables_still_order_when_a_cycle_exists_elsewhere() {
        let s = schema(&[
            ("a", &[("b", false)]),
            ("b", &[("a", false)]),
            ("countries", &[]),
            ("cities", &[("countries", false)]),
        ]);
        let o = topological(&s, &ids(&["a", "b", "countries", "cities"]));
        assert_eq!(o.cycles.len(), 1);
        let n = names(&o.tables);
        let ci = n.iter().position(|x| x == "countries").unwrap();
        let cj = n.iter().position(|x| x == "cities").unwrap();
        assert!(
            ci < cj,
            "the acyclic part must still be correctly ordered: {n:?}"
        );
        assert_eq!(o.tables.len(), 4, "every selected table must appear");
    }

    #[test]
    fn diamond_dependency_orders_correctly() {
        let s = schema(&[
            ("top", &[]),
            ("left", &[("top", false)]),
            ("right", &[("top", false)]),
            ("bottom", &[("left", false), ("right", false)]),
        ]);
        let o = topological(&s, &ids(&["bottom", "left", "right", "top"]));
        assert!(o.cycles.is_empty());
        assert_eq!(names(&o.tables), ["top", "left", "right", "bottom"]);
    }

    #[test]
    fn unqualified_selection_resolves_against_the_default_schema() {
        let s = schema(&[("orders", &[("users", false)]), ("users", &[])]);
        let o = topological(&s, &[TableId::bare("orders"), TableId::bare("users")]);
        assert_eq!(names(&o.tables), ["users", "orders"]);
        // And the output is canonicalised to the qualified form.
        assert!(
            o.tables
                .iter()
                .all(|t| t.schema.as_deref() == Some("public"))
        );
    }

    #[test]
    fn duplicate_selection_is_deduped() {
        let s = schema(&[("users", &[])]);
        let o = topological(
            &s,
            &[TableId::bare("users"), TableId::new("public", "users")],
        );
        assert_eq!(o.tables.len(), 1);
    }
}

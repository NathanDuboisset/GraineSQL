//! `graine.lock`, the committed snapshot of the schema the seed files were
//! written against.
//!
//! Three sections, kept deliberately separate so schema drift and data drift are
//! distinct signals: `order` (the load manifest), `schema` (what every command
//! checks the live database against), and `files` (content hashes, checked by
//! `graine verify`).

pub mod drift;

use std::path::Path;

use anyhow::{Context, Result, bail};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::config::Engine;
use crate::schema::{Schema, Table, TableId};

/// Version 2 changed what the fingerprint covers, so every v1 one is stale.
pub const LOCK_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// Path relative to the output directory.
    pub path: String,
    pub rows: u64,
    /// Hex sha256 of the file's bytes; for a per-row table, of the concatenated
    /// per-row files in load order.
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lock {
    pub version: u32,
    pub engine: Engine,
    /// Hash over the whole schema section. A single value to compare when all
    /// you need is a yes/no.
    pub fingerprint: String,
    /// Load order: parents first. This is why data files can keep plain names.
    pub order: Vec<TableId>,
    pub schema: IndexMap<TableId, Table>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub enums: IndexMap<String, Vec<String>>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub files: IndexMap<TableId, FileEntry>,
    /// Storage bucket *settings*, keyed by bucket id.
    ///
    /// Schema, not data: buckets are created by migrations. GraineSQL records them
    /// to check the target against and never writes them.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub buckets: IndexMap<String, crate::storage::BucketSettings>,
    /// Bucket contents, keyed by bucket id. The data counterpart to `files`.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub bucket_files: IndexMap<String, BucketEntry>,
    /// Drift the user chose to stop being asked about, keyed by table. In the
    /// lock rather than a sidecar so the decision shows up in review.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub accepted: IndexMap<String, Vec<String>>,
}

/// A bucket's recorded contents, for `graine verify`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BucketEntry {
    pub objects: u64,
    pub bytes: u64,
    /// Hex sha256 over the manifest, which in turn holds every object's own
    /// sha256, path, and size.
    pub sha256: String,
}

impl Lock {
    /// Build a lock from a freshly introspected schema and a load order.
    pub fn build(engine: Engine, schema: &Schema, order: &[TableId]) -> Lock {
        let mut lock = Lock {
            version: LOCK_VERSION,
            engine,
            fingerprint: String::new(),
            order: order
                .iter()
                .map(|id| relative(id, &schema.default_schema))
                .collect(),
            schema: relative_tables(schema),
            enums: schema.enums.clone(),
            files: IndexMap::new(),
            buckets: IndexMap::new(),
            bucket_files: IndexMap::new(),
            accepted: IndexMap::new(),
        };
        lock.fingerprint = fingerprint_schema(schema);
        lock
    }

    /// Whether `change` on `scope` was accepted, specifically or by a `*`.
    pub fn is_accepted(&self, scope: &str, change: &str) -> bool {
        self.accepted
            .get(scope)
            .is_some_and(|c| c.iter().any(|e| e == "*" || e == change))
    }

    pub fn accept(&mut self, scope: &str, change: &str) {
        let entry = self.accepted.entry(scope.to_string()).or_default();
        // A blanket acceptance subsumes every specific one on the same table.
        if change == "*" {
            entry.clear();
        } else if entry.iter().any(|e| e == "*") {
            return;
        }
        if !entry.iter().any(|e| e == change) {
            entry.push(change.to_string());
            entry.sort();
        }
        self.accepted.sort_keys();
    }

    /// Re-locking absorbs the drift, so specific acceptances are now dead. Only
    /// a blanket `*`, a standing decision about a table, survives.
    pub fn carry_accepted(&mut self, previous: &Lock, keep: impl Fn(&str) -> bool) {
        self.accepted = previous
            .accepted
            .iter()
            .filter(|(scope, changes)| changes.iter().any(|c| c == "*") && keep(scope))
            .map(|(scope, _)| (scope.clone(), vec!["*".to_string()]))
            .collect();
    }

    /// The schema section as a [`Schema`], qualified against `default_schema`.
    ///
    /// Tables are stored relative to whatever schema they came from, so the
    /// same structure locks identically whatever the database is called. On
    /// MySQL the database name *is* the schema name, which makes it matter.
    pub fn to_schema_in(&self, default_schema: &str) -> Schema {
        let mut s = Schema {
            default_schema: default_schema.to_string(),
            tables: self.schema.clone(),
            enums: self.enums.clone(),
        };
        s.rebase("", default_schema);
        s.hydrate();
        s
    }

    /// The schema section, qualified to match a live schema.
    pub fn to_schema_for(&self, live: &Schema) -> Schema {
        self.to_schema_in(&live.default_schema)
    }

    /// The load order, qualified against `default_schema`.
    pub fn order_in(&self, default_schema: &str) -> Vec<TableId> {
        self.order
            .iter()
            .map(|id| match id.schema.as_deref() {
                None => TableId::new(default_schema, &id.name),
                Some(_) => id.clone(),
            })
            .collect()
    }

    pub fn read(path: &Path) -> Result<Lock> {
        let text = std::fs::read_to_string(path).with_context(|| {
            format!(
                "reading lock file {} (run `graine lock` to create it)",
                path.display()
            )
        })?;
        let mut lock: Lock = serde_yaml_ng::from_str(&text)
            .with_context(|| format!("parsing lock file {}", path.display()))?;
        if lock.version != LOCK_VERSION {
            bail!(
                "lock file {} is version {} but this build writes version {LOCK_VERSION}; \
                 re-run `graine lock`",
                path.display(),
                lock.version
            );
        }
        for (id, table) in lock.schema.iter_mut() {
            table.id = id.clone();
        }
        Ok(lock)
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let mut text = serde_yaml_ng::to_string(self).context("serializing the lock file")?;
        if !text.ends_with('\n') {
            text.push('\n');
        }
        crate::io::write_atomic(path, text.as_bytes())
            .with_context(|| format!("writing lock file {}", path.display()))
    }

    /// Reorder `live`'s columns to match this lock, so the physical column
    /// order of a particular database cannot leak into the output.
    ///
    /// Two databases holding the same columns in a different physical order must
    /// still export identically. Columns the lock does not know about are
    /// appended in live order, so a newly added one is still exported.
    pub fn align_column_order(&self, live: &mut Schema) {
        let default = live.default_schema.clone();
        for (id, table) in live.tables.iter_mut() {
            let Some(locked) = self.schema.get(&relative(id, &default)) else {
                continue;
            };
            let mut ordered: Vec<crate::schema::Column> = Vec::with_capacity(table.columns.len());
            for lc in &locked.columns {
                if let Some(pos) = table.columns.iter().position(|c| c.name == lc.name) {
                    ordered.push(table.columns.remove(pos));
                }
            }
            // Whatever is left is new since the lock was written.
            ordered.append(&mut table.columns);
            table.columns = ordered;
        }
    }
}

/// Drop the default schema from an id, so the lock does not record which
/// database it happened to come from.
pub fn relative(id: &TableId, default_schema: &str) -> TableId {
    match id.schema.as_deref() {
        Some(s) if s == default_schema => TableId::bare(&id.name),
        None => TableId::bare(&id.name),
        Some(_) => id.clone(),
    }
}

fn relative_tables(schema: &Schema) -> IndexMap<TableId, Table> {
    schema
        .tables
        .iter()
        .map(|(id, table)| {
            let mut table = table.clone();
            for fk in &mut table.foreign_keys {
                fk.references = relative(&fk.references, &schema.default_schema);
            }
            let id = relative(id, &schema.default_schema);
            table.id = id.clone();
            (id, table)
        })
        .collect()
}

/// Hash a schema into a short, stable identifier.
///
/// Built from a canonical rendering rather than the serialized YAML so that
/// formatting changes in the writer never look like schema drift.
pub fn fingerprint_schema(schema: &Schema) -> String {
    let mut hasher = blake3::Hasher::new();
    // The database name is deliberately excluded: the same structure must
    // fingerprint the same wherever it lives, or `lock --check` in CI fails on
    // any database not named like the one the lock was taken from.
    for (id, table) in &relative_tables(schema) {
        hasher.update(id.to_string().as_bytes());
        hasher.update(b"\n");
        hasher.update(fingerprint_table(table).as_bytes());
        hasher.update(b"\n");
    }
    for (name, labels) in &schema.enums {
        hasher.update(b"enum ");
        hasher.update(name.as_bytes());
        hasher.update(b"=");
        hasher.update(labels.join(",").as_bytes());
        hasher.update(b"\n");
    }
    short_hash(hasher.finalize().to_hex().as_str())
}

pub fn fingerprint_table(table: &Table) -> String {
    let mut hasher = blake3::Hasher::new();
    // Sorted by name, not taken in physical order: drift treats a column
    // reorder as a non-event, so the fingerprint has to agree, or `lock --check`
    // would demand a re-lock for a change that changes nothing.
    let mut columns: Vec<&crate::schema::Column> = table.columns.iter().collect();
    columns.sort_by(|a, b| a.name.cmp(&b.name));
    for c in columns {
        // `sql_type` is deliberately absent: it is the engine's own spelling,
        // which no drift rule enforces, and hashing it stopped a lock ever
        // matching a database on another engine.
        hasher.update(
            format!(
                "col {} {} {} {} {}\n",
                c.name,
                c.class.label(),
                c.nullable,
                c.has_default,
                c.generated,
            )
            .as_bytes(),
        );
    }
    hasher.update(format!("pk {}\n", table.primary_key.join(",")).as_bytes());
    // Unique constraints and foreign keys are sets, not sequences, so sort them
    // too rather than trusting catalog iteration order.
    let mut unique: Vec<&crate::schema::UniqueKey> = table.unique.iter().collect();
    unique.sort();
    for u in unique {
        hasher.update(format!("uq {}", u.columns.join(",")).as_bytes());
        if let Some(p) = &u.predicate {
            hasher.update(format!(" where {p}").as_bytes());
        }
        hasher.update(b"\n");
    }
    let mut fks: Vec<&crate::schema::ForeignKey> = table.foreign_keys.iter().collect();
    fks.sort_by_key(|f| f.columns.clone());
    for fk in fks {
        hasher.update(
            format!(
                "fk {} -> {} {} deferrable={}\n",
                fk.columns.join(","),
                fk.references,
                fk.ref_columns.join(","),
                fk.deferrable,
            )
            .as_bytes(),
        );
    }
    short_hash(hasher.finalize().to_hex().as_str())
}

fn short_hash(hex: &str) -> String {
    format!("b3:{}", &hex[..16])
}

/// Hex sha256 of a byte slice, the file-content hash the lock records.
///
/// sha256 rather than blake3 here so a user can verify a seed file with the
/// `sha256sum` already on their machine.
pub fn file_hash(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Column, ForeignKey, TypeClass, UniqueKey};

    fn col(name: &str, class: TypeClass, nullable: bool) -> Column {
        Column {
            name: name.into(),
            sql_type: class.label(),
            class,
            nullable,
            has_default: false,
            generated: false,
            identity: false,
        }
    }

    fn table(name: &str) -> Table {
        Table {
            id: TableId::new("public", name),
            columns: vec![
                col("id", TypeClass::Int { bits: 64 }, false),
                col("email", TypeClass::Text { max_len: None }, true),
            ],
            primary_key: vec!["id".into()],
            unique: vec![],
            foreign_keys: vec![],
        }
    }

    fn schema(tables: Vec<Table>) -> Schema {
        let mut map = IndexMap::new();
        for t in tables {
            map.insert(t.id.clone(), t);
        }
        Schema {
            default_schema: "public".into(),
            tables: map,
            enums: IndexMap::new(),
        }
    }

    #[test]
    fn fingerprint_is_stable_across_runs() {
        let s = schema(vec![table("users")]);
        assert_eq!(fingerprint_schema(&s), fingerprint_schema(&s.clone()));
    }

    #[test]
    fn fingerprint_changes_when_the_schema_does() {
        let base = schema(vec![table("users")]);
        let before = fingerprint_schema(&base);

        let mut widened = base.clone();
        widened.tables[0].columns[1].class = TypeClass::Text { max_len: Some(50) };
        assert_ne!(
            before,
            fingerprint_schema(&widened),
            "a type change must show"
        );

        let mut renullabled = base.clone();
        renullabled.tables[0].columns[1].nullable = false;
        assert_ne!(
            before,
            fingerprint_schema(&renullabled),
            "nullability must show"
        );

        let mut repked = base.clone();
        repked.tables[0].primary_key = vec!["email".into()];
        assert_ne!(before, fingerprint_schema(&repked), "a pk change must show");
    }

    #[test]
    fn fingerprint_ignores_column_order() {
        // Columns are matched by name everywhere, so a physical reordering is
        // not something a user should have to re-lock for.
        let base = schema(vec![table("users")]);
        let mut reordered = base.clone();
        reordered.tables[0].columns.swap(0, 1);
        assert_eq!(fingerprint_schema(&base), fingerprint_schema(&reordered));
    }

    #[test]
    fn fingerprint_ignores_unique_and_foreign_key_ordering() {
        let mut a = schema(vec![table("users")]);
        a.tables[0].unique = vec![
            UniqueKey::total(vec!["email".into()]),
            UniqueKey::total(vec!["id".into()]),
        ];
        let mut b = a.clone();
        b.tables[0].unique.reverse();
        assert_eq!(fingerprint_schema(&a), fingerprint_schema(&b));
    }

    #[test]
    fn fingerprint_covers_foreign_keys_and_enums() {
        let mut base = schema(vec![table("users")]);
        let before = fingerprint_schema(&base);

        base.tables[0].foreign_keys.push(ForeignKey {
            name: "fk".into(),
            columns: vec!["org_id".into()],
            references: TableId::new("public", "orgs"),
            ref_columns: vec!["id".into()],
            deferrable: false,
        });
        let with_fk = fingerprint_schema(&base);
        assert_ne!(before, with_fk);

        base.enums
            .insert("tier".into(), vec!["free".into(), "pro".into()]);
        assert_ne!(with_fk, fingerprint_schema(&base));
    }

    #[test]
    fn lock_round_trips_through_yaml_and_refills_ids() {
        let s = schema(vec![table("users"), table("orgs")]);
        let order = vec![
            TableId::new("public", "orgs"),
            TableId::new("public", "users"),
        ];
        let lock = Lock::build(Engine::Postgres, &s, &order);

        let yaml = serde_yaml_ng::to_string(&lock).unwrap();
        let mut back: Lock = serde_yaml_ng::from_str(&yaml).unwrap();
        for (id, t) in back.schema.iter_mut() {
            t.id = id.clone();
        }
        assert_eq!(lock, back);
        // The id is reconstructed from the key rather than duplicated on disk.
        assert!(
            !yaml.contains("id: public.users"),
            "id should not be serialized:\n{yaml}"
        );
        assert_eq!(back.schema[&TableId::bare("users")].id.name, "users");
    }

    #[test]
    fn the_lock_stores_names_relative_to_the_default_schema() {
        let s = schema(vec![table("users"), table("orgs")]);
        let order = vec![
            TableId::new("public", "orgs"),
            TableId::new("public", "users"),
        ];
        let lock = Lock::build(Engine::Postgres, &s, &order);

        // Stored bare, so the lock does not record which database it came from.
        assert_eq!(
            lock.order,
            vec![TableId::bare("orgs"), TableId::bare("users")]
        );
        assert!(lock.schema.contains_key(&TableId::bare("users")));

        assert_eq!(lock.order_in("public"), order);
        assert_eq!(lock.to_schema_in("public").tables.len(), 2);
        assert!(
            lock.to_schema_in("app")
                .tables
                .contains_key(&TableId::new("app", "users"))
        );
    }

    #[test]
    fn the_same_structure_fingerprints_the_same_in_any_database() {
        // Otherwise `lock --check` fails in CI against any database not named
        // like the one the lock was taken from, which on MySQL is every one.
        let mut a = schema(vec![table("users")]);
        let mut b = a.clone();
        b.rebase("public", "graine_dst");

        assert_eq!(fingerprint_schema(&a), fingerprint_schema(&b));

        a.tables[0].columns.pop();
        assert_ne!(fingerprint_schema(&a), fingerprint_schema(&b));
    }

    #[test]
    fn align_column_order_makes_the_lock_authoritative() {
        // The lock says (id, email); the live database has them the other way
        // round, as happens after a drop-and-re-add.
        let locked = schema(vec![table("users")]);
        let lock = Lock::build(Engine::Postgres, &locked, &[]);

        let mut live = locked.clone();
        live.tables[0].columns.swap(0, 1);
        assert_eq!(live.tables[0].columns[0].name, "email");

        lock.align_column_order(&mut live);
        let names: Vec<&str> = live.tables[0]
            .columns
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(names, ["id", "email"], "the lock must decide the order");
    }

    #[test]
    fn align_column_order_appends_columns_the_lock_does_not_know() {
        let locked = schema(vec![table("users")]);
        let lock = Lock::build(Engine::Postgres, &locked, &[]);

        let mut live = locked.clone();
        // A new column, inserted physically first.
        live.tables[0]
            .columns
            .insert(0, col("phone", TypeClass::Text { max_len: None }, true));

        lock.align_column_order(&mut live);
        let names: Vec<&str> = live.tables[0]
            .columns
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(
            names,
            ["id", "email", "phone"],
            "known columns keep lock order, new ones follow"
        );
    }

    #[test]
    fn align_column_order_ignores_tables_the_lock_has_never_seen() {
        let lock = Lock::build(Engine::Postgres, &schema(vec![table("users")]), &[]);
        let mut live = schema(vec![table("orgs")]);
        let before = live.clone();
        lock.align_column_order(&mut live);
        assert_eq!(live, before);
    }

    #[test]
    fn file_hash_matches_sha256sum() {
        // Known vector, so a reader can check a seed file with coreutils.
        assert_eq!(
            file_hash(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            file_hash(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}

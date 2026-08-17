//! `seedle.lock` — the committed snapshot of the schema the seed files were
//! written against.
//!
//! Three sections, kept deliberately separate so schema drift and data drift are
//! distinct signals: `order` (the load manifest), `schema` (what every command
//! checks the live database against), and `files` (content hashes, checked by
//! `seedle verify`).

pub mod drift;

use std::path::Path;

use anyhow::{Context, Result, bail};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::config::Engine;
use crate::schema::{Schema, Table, TableId};

pub const LOCK_VERSION: u32 = 1;

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
    /// The default schema of the source this lock was taken from.
    pub default_schema: String,
    pub schema: IndexMap<TableId, Table>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub enums: IndexMap<String, Vec<String>>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub files: IndexMap<TableId, FileEntry>,
    /// Storage bucket *settings*, keyed by bucket id.
    ///
    /// This is schema, not data: buckets are created and configured by
    /// migrations, so seedle records them here only to check the target against
    /// — it never creates or reconfigures one.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub buckets: IndexMap<String, crate::storage::BucketSettings>,
    /// Bucket contents, keyed by bucket id. The data counterpart to `files`.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub bucket_files: IndexMap<String, BucketEntry>,
}

/// A bucket's recorded contents, for `seedle verify`.
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
            order: order.to_vec(),
            default_schema: schema.default_schema.clone(),
            schema: schema.tables.clone(),
            enums: schema.enums.clone(),
            files: IndexMap::new(),
            buckets: IndexMap::new(),
            bucket_files: IndexMap::new(),
        };
        lock.fingerprint = fingerprint_schema(schema);
        lock
    }

    /// The schema section as a [`Schema`], for comparison against a live one.
    pub fn to_schema(&self) -> Schema {
        let mut s = Schema {
            default_schema: self.default_schema.clone(),
            tables: self.schema.clone(),
            enums: self.enums.clone(),
        };
        s.hydrate();
        s
    }

    pub fn read(path: &Path) -> Result<Lock> {
        let text = std::fs::read_to_string(path).with_context(|| {
            format!(
                "reading lock file {} (run `seedle lock` to create it)",
                path.display()
            )
        })?;
        let mut lock: Lock = serde_yaml_ng::from_str(&text)
            .with_context(|| format!("parsing lock file {}", path.display()))?;
        if lock.version != LOCK_VERSION {
            bail!(
                "lock file {} is version {} but this build writes version {LOCK_VERSION}; \
                 re-run `seedle lock`",
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
    /// Two databases that hold the same columns in a different physical order —
    /// which is what happens as soon as one of them has had a column added and
    /// re-added — must still export byte-identical files. The lock is the
    /// authority; columns it does not know about are appended in live order so
    /// a newly added column is still exported.
    pub fn align_column_order(&self, live: &mut Schema) {
        for (id, table) in live.tables.iter_mut() {
            let Some(locked) = self.schema.get(id) else {
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

/// Hash a schema into a short, stable identifier.
///
/// Built from a canonical rendering rather than the serialized YAML so that
/// formatting changes in the writer never look like schema drift.
pub fn fingerprint_schema(schema: &Schema) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(schema.default_schema.as_bytes());
    hasher.update(b"\n");
    // IndexMap preserves introspection's sorted order, so this is reproducible.
    for (id, table) in &schema.tables {
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
        hasher.update(
            format!(
                "col {} {} {} {} {} {}\n",
                c.name,
                c.class.label(),
                c.sql_type,
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
    let mut unique: Vec<&Vec<String>> = table.unique.iter().collect();
    unique.sort();
    for u in unique {
        hasher.update(format!("uq {}\n", u.join(",")).as_bytes());
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

/// Hex sha256 of a byte slice — the file-content hash the lock records.
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
    use crate::schema::{Column, ForeignKey, TypeClass};

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
        a.tables[0].unique = vec![vec!["email".into()], vec!["id".into()]];
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
        assert_eq!(
            back.schema[&TableId::new("public", "users")].id.name,
            "users"
        );
    }

    #[test]
    fn lock_preserves_order_exactly() {
        let s = schema(vec![table("users"), table("orgs")]);
        let order = vec![
            TableId::new("public", "orgs"),
            TableId::new("public", "users"),
        ];
        let lock = Lock::build(Engine::Postgres, &s, &order);
        assert_eq!(lock.order, order);
        assert_eq!(lock.to_schema().tables.len(), 2);
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

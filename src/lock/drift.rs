//! Classifying the difference between the locked schema and the live one.
//!
//! The split that matters: *breaking* drift would reject or corrupt data, so it
//! aborts before anything is touched; *benign* drift cannot, so it warns and
//! lets the command proceed. Getting a change into the wrong bucket is the worst
//! failure mode this tool has, a false benign lets a bad load through, a false
//! breaking blocks work for no reason, so every rule below has a test.

use std::collections::BTreeSet;
use std::fmt;

use indexmap::IndexMap;

use crate::schema::{Column, ForeignKey, Schema, Table, TableId, TypeClass};
use crate::storage::BucketSettings;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Would reject or corrupt data. Abort.
    Breaking,
    /// Loses data that the seed files carry, but succeeds. Needs confirmation.
    Confirm,
    /// Cannot affect a load of the existing seed files. Proceed.
    Benign,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Severity::Breaking => "breaking",
            Severity::Confirm => "needs confirmation",
            Severity::Benign => "benign",
        }
    }
}

/// What a drift entry is about.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Target {
    Table(TableId),
    Column(TableId, String),
    /// A storage bucket. Buckets are created by migrations, so seedle only ever
    /// checks them, it never creates or reconfigures one.
    Bucket(String),
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Target::Table(t) => write!(f, "{t}"),
            Target::Column(t, c) => write!(f, "{t}.{c}"),
            Target::Bucket(b) => write!(f, "bucket {b}"),
        }
    }
}

impl Target {
    /// The table this concerns, for machine-readable output.
    pub fn table(&self) -> Option<&TableId> {
        match self {
            Target::Table(t) | Target::Column(t, _) => Some(t),
            Target::Bucket(_) => None,
        }
    }

    pub fn column(&self) -> Option<&str> {
        match self {
            Target::Column(_, c) => Some(c),
            _ => None,
        }
    }

    pub fn bucket(&self) -> Option<&str> {
        match self {
            Target::Bucket(b) => Some(b),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drift {
    pub severity: Severity,
    pub target: Target,
    /// What changed, in one line.
    pub what: String,
    /// Why it is classified this way, and what to do. Empty for benign items
    /// that need no explanation.
    pub note: String,
}

impl fmt::Display for Drift {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}  {}", self.target, self.what)
    }
}

/// The full comparison result.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub drifts: Vec<Drift>,
}

impl Report {
    pub fn breaking(&self) -> impl Iterator<Item = &Drift> {
        self.drifts
            .iter()
            .filter(|d| d.severity == Severity::Breaking)
    }

    pub fn confirm(&self) -> impl Iterator<Item = &Drift> {
        self.drifts
            .iter()
            .filter(|d| d.severity == Severity::Confirm)
    }

    pub fn needs_confirmation(&self) -> bool {
        self.confirm().next().is_some()
    }

    pub fn benign(&self) -> impl Iterator<Item = &Drift> {
        self.drifts
            .iter()
            .filter(|d| d.severity == Severity::Benign)
    }

    pub fn has_breaking(&self) -> bool {
        self.breaking().next().is_some()
    }

    pub fn is_empty(&self) -> bool {
        self.drifts.is_empty()
    }

    /// (breaking, needs confirmation, benign)
    pub fn counts(&self) -> (usize, usize, usize) {
        (
            self.breaking().count(),
            self.confirm().count(),
            self.benign().count(),
        )
    }

    /// Human-readable report, aligned into columns.
    pub fn render(&self) -> String {
        if self.is_empty() {
            return "schema matches seedle.lock\n".to_string();
        }
        let mut out = String::new();
        let width = self
            .drifts
            .iter()
            .map(|d| d.target.to_string().chars().count())
            .max()
            .unwrap_or(0)
            .min(48);

        for severity in [Severity::Breaking, Severity::Confirm, Severity::Benign] {
            let group: Vec<&Drift> = self
                .drifts
                .iter()
                .filter(|d| d.severity == severity)
                .collect();
            if group.is_empty() {
                continue;
            }
            out.push_str(&format!("\n{}:\n", severity.label()));
            for d in group {
                out.push_str(&format!(
                    "  {:width$}  {}\n",
                    d.target.to_string(),
                    d.what,
                    width = width
                ));
                if !d.note.is_empty() {
                    out.push_str(&format!("  {:width$}    {}\n", "", d.note, width = width));
                }
            }
        }

        let (breaking, confirm, benign) = self.counts();
        out.push_str(&format!(
            "\n{breaking} breaking, {confirm} needing confirmation, {benign} benign.{}\n",
            if breaking > 0 {
                " Review, then re-run `seedle lock` to accept."
            } else {
                ""
            }
        ));
        out
    }
}

/// Compare the locked schema against a freshly introspected one.
///
/// `locked` is the contract the committed seed files were written against;
/// `live` is what the database looks like now.
pub fn classify(locked: &Schema, live: &Schema) -> Report {
    let mut drifts = Vec::new();

    for (id, locked_table) in &locked.tables {
        match live.get(id) {
            None => drifts.push(Drift {
                severity: Severity::Confirm,
                target: Target::Table(id.clone()),
                what: "table dropped".into(),
                note: "its seed rows will be discarded".into(),
            }),
            Some(live_table) => {
                compare_table(id, locked_table, live_table, &mut drifts);
            }
        }
    }

    for id in live.tables.keys() {
        if locked.get(id).is_none() {
            drifts.push(Drift {
                severity: Severity::Benign,
                target: Target::Table(id.clone()),
                what: "table added".into(),
                // Only tables listed in the config are ever touched, so a new
                // table cannot affect an existing load.
                note: String::new(),
            });
        }
    }

    compare_enums(locked, live, &mut drifts);

    // Breaking first, then by table, so the most important line is at the top
    // and the ordering is stable across runs.
    drifts.sort_by(|a, b| {
        a.severity
            .cmp(&b.severity)
            .then_with(|| a.target.cmp(&b.target))
            .then_with(|| a.what.cmp(&b.what))
    });

    Report { drifts }
}

fn compare_table(id: &TableId, locked: &Table, live: &Table, out: &mut Vec<Drift>) {
    for lc in &locked.columns {
        match live.column(&lc.name) {
            None => out.push(Drift {
                // The load still succeeds, it just silently stops carrying this
                // column's data. That is a judgement call, not an error.
                severity: Severity::Confirm,
                target: Target::Column(id.clone(), lc.name.clone()),
                what: format!("column dropped (was {})", lc.class.label()),
                note: "the seed files carry data for it, which will be discarded".into(),
            }),
            Some(vc) => compare_column(id, lc, vc, out),
        }
    }

    for vc in &live.columns {
        if locked.column(&vc.name).is_some() {
            continue;
        }
        let can_omit = vc.nullable || vc.has_default || vc.generated;
        out.push(Drift {
            severity: if can_omit {
                Severity::Benign
            } else {
                Severity::Breaking
            },
            target: Target::Column(id.clone(), vc.name.clone()),
            what: format!(
                "column added ({}{})",
                vc.class.label(),
                if vc.nullable {
                    ", nullable"
                } else if vc.has_default {
                    ", NOT NULL with default"
                } else if vc.generated {
                    ", generated"
                } else {
                    ", NOT NULL with no default"
                }
            ),
            note: if can_omit {
                String::new()
            } else {
                "every insert from the seed files would omit it and be rejected".into()
            },
        });
    }

    if locked.primary_key != live.primary_key {
        out.push(Drift {
            severity: Severity::Breaking,
            target: Target::Table(id.clone()),
            what: format!(
                "primary key {} -> {}",
                fmt_key(&locked.primary_key),
                fmt_key(&live.primary_key)
            ),
            note: "upserts conflict-target the primary key".into(),
        });
    }

    let locked_uq: BTreeSet<&Vec<String>> = locked.unique.iter().collect();
    let live_uq: BTreeSet<&Vec<String>> = live.unique.iter().collect();
    for gone in locked_uq.difference(&live_uq) {
        out.push(Drift {
            severity: Severity::Breaking,
            target: Target::Table(id.clone()),
            what: format!("unique constraint dropped on {}", fmt_key(gone)),
            note: "it may be the conflict target an upsert relies on".into(),
        });
    }
    for added in live_uq.difference(&locked_uq) {
        out.push(Drift {
            severity: Severity::Benign,
            target: Target::Table(id.clone()),
            what: format!("unique constraint added on {}", fmt_key(added)),
            note: String::new(),
        });
    }

    compare_foreign_keys(id, locked, live, out);
}

fn compare_column(id: &TableId, locked: &Column, live: &Column, out: &mut Vec<Drift>) {
    if locked.class != live.class {
        let widens = locked.class.widens_to(&live.class);
        out.push(Drift {
            severity: if widens {
                Severity::Benign
            } else {
                Severity::Breaking
            },
            target: Target::Column(id.clone(), locked.name.clone()),
            what: format!("{} -> {}", locked.class.label(), live.class.label()),
            note: if widens {
                String::new()
            } else {
                "existing seed values may not fit or may fail to parse".into()
            },
        });
    }

    if locked.nullable && !live.nullable {
        // A default lets the database fill an omitted column, but an explicit
        // null in a seed file still fails; the file check at load time catches
        // that precisely, so a default downgrades this to a warning.
        let has_default = live.has_default;
        out.push(Drift {
            severity: if has_default {
                Severity::Benign
            } else {
                Severity::Breaking
            },
            target: Target::Column(id.clone(), locked.name.clone()),
            what: "nullable -> NOT NULL".into(),
            note: if has_default {
                "it has a default, but rows with an explicit null will still be rejected".into()
            } else {
                "seed rows holding null for it would be rejected".into()
            },
        });
    }

    if !locked.nullable && live.nullable {
        out.push(Drift {
            severity: Severity::Benign,
            target: Target::Column(id.clone(), locked.name.clone()),
            what: "NOT NULL -> nullable".into(),
            note: String::new(),
        });
    }

    if !locked.generated && live.generated {
        out.push(Drift {
            severity: Severity::Breaking,
            target: Target::Column(id.clone(), locked.name.clone()),
            what: "column became generated".into(),
            note: "a generated column cannot be written to".into(),
        });
    }
}

fn compare_foreign_keys(id: &TableId, locked: &Table, live: &Table, out: &mut Vec<Drift>) {
    // Match by the columns the key is on rather than by constraint name, so a
    // rename is not mistaken for a drop plus an add.
    for lfk in &locked.foreign_keys {
        match live.foreign_keys.iter().find(|f| f.columns == lfk.columns) {
            None => out.push(Drift {
                severity: Severity::Breaking,
                target: Target::Table(id.clone()),
                what: format!("foreign key on {} dropped", fmt_key(&lfk.columns)),
                note: "load order was computed from it".into(),
            }),
            Some(vfk) => {
                if vfk.references != lfk.references || vfk.ref_columns != lfk.ref_columns {
                    out.push(Drift {
                        severity: Severity::Breaking,
                        target: Target::Table(id.clone()),
                        what: format!(
                            "foreign key on {} retargeted: {} -> {}",
                            fmt_key(&lfk.columns),
                            fk_target(lfk),
                            fk_target(vfk)
                        ),
                        note: "load order and row acceptance both depend on the target".into(),
                    });
                }
            }
        }
    }

    for vfk in &live.foreign_keys {
        if locked.foreign_keys.iter().any(|f| f.columns == vfk.columns) {
            continue;
        }
        out.push(Drift {
            severity: Severity::Breaking,
            target: Target::Table(id.clone()),
            what: format!(
                "foreign key added on {} -> {}",
                fmt_key(&vfk.columns),
                fk_target(vfk)
            ),
            note: "existing seed rows may reference parents that will not exist".into(),
        });
    }
}

fn compare_enums(locked: &Schema, live: &Schema, out: &mut Vec<Drift>) {
    for (name, locked_labels) in &locked.enums {
        let Some(live_labels) = live.enums.get(name) else {
            // The type is gone entirely; the column type change already reports
            // this, so do not double-report unless no column mentions it.
            if !any_column_uses_enum(locked, name) {
                continue;
            }
            out.push(Drift {
                severity: Severity::Breaking,
                target: Target::Table(enum_owner(locked, name)),
                what: format!("enum type {name} dropped"),
                note: String::new(),
            });
            continue;
        };

        let live_set: BTreeSet<&String> = live_labels.iter().collect();
        for label in locked_labels {
            if !live_set.contains(label) {
                out.push(Drift {
                    severity: Severity::Breaking,
                    target: Target::Table(enum_owner(locked, name)),
                    what: format!("enum {name}: label {label:?} removed"),
                    note: "seed rows holding that label can no longer be loaded".into(),
                });
            }
        }
        let locked_set: BTreeSet<&String> = locked_labels.iter().collect();
        for label in live_labels {
            if !locked_set.contains(label) {
                out.push(Drift {
                    severity: Severity::Benign,
                    target: Target::Table(enum_owner(locked, name)),
                    what: format!("enum {name}: label {label:?} added"),
                    note: String::new(),
                });
            }
        }
    }
}

fn any_column_uses_enum(schema: &Schema, name: &str) -> bool {
    schema
        .tables
        .values()
        .flat_map(|t| t.columns.iter())
        .any(|c| uses_enum(&c.class, name))
}

fn uses_enum(class: &TypeClass, name: &str) -> bool {
    match class {
        TypeClass::Enum { name: n } => n == name,
        TypeClass::Array { of } => uses_enum(of, name),
        _ => false,
    }
}

/// A table that uses this enum, so the drift line has somewhere to hang.
fn enum_owner(schema: &Schema, name: &str) -> TableId {
    schema
        .tables
        .iter()
        .find(|(_, t)| t.columns.iter().any(|c| uses_enum(&c.class, name)))
        .map(|(id, _)| id.clone())
        .unwrap_or_else(|| TableId::bare(format!("(enum {name})")))
}

/// Compare locked bucket settings against the live ones.
///
/// Buckets are created and configured by migrations, so seedle never writes
/// these, it only reports when they no longer match what the seed files were
/// exported against. The question each rule answers is the same as for tables:
/// would this make the existing objects fail to load?
pub fn classify_buckets(
    locked: &IndexMap<String, BucketSettings>,
    live: &IndexMap<String, BucketSettings>,
    largest_object: &IndexMap<String, u64>,
) -> Vec<Drift> {
    let mut out = Vec::new();

    for (name, l) in locked {
        let Some(v) = live.get(name) else {
            out.push(Drift {
                severity: Severity::Breaking,
                target: Target::Bucket(name.clone()),
                what: "bucket does not exist".into(),
                note: "buckets are created by migrations; run them against this project".into(),
            });
            continue;
        };

        // A size limit below the largest exported object rejects that upload.
        if let Some(limit) = v.file_size_limit {
            let biggest = largest_object.get(name).copied().unwrap_or(0);
            let was = l.file_size_limit.unwrap_or(u64::MAX);
            if limit < biggest {
                out.push(Drift {
                    severity: Severity::Breaking,
                    target: Target::Bucket(name.clone()),
                    what: format!(
                        "file size limit {} is below the largest exported object ({biggest} bytes)",
                        limit
                    ),
                    note: "that object would be rejected on upload".into(),
                });
            } else if limit < was {
                out.push(Drift {
                    severity: Severity::Benign,
                    target: Target::Bucket(name.clone()),
                    what: format!("file size limit lowered to {limit}"),
                    note: String::new(),
                });
            }
        }

        // A mime allowlist that no longer covers an exported object's type.
        if l.allowed_mime_types != v.allowed_mime_types {
            out.push(Drift {
                severity: Severity::Benign,
                target: Target::Bucket(name.clone()),
                what: format!(
                    "allowed mime types {} -> {}",
                    fmt_mimes(&l.allowed_mime_types),
                    fmt_mimes(&v.allowed_mime_types)
                ),
                note: "an upload whose type is now excluded would be rejected".into(),
            });
        }

        // Visibility does not affect whether an object can be written.
        if l.public != v.public {
            out.push(Drift {
                severity: Severity::Benign,
                target: Target::Bucket(name.clone()),
                what: format!("public {} -> {}", l.public, v.public),
                note: String::new(),
            });
        }
    }

    for name in live.keys() {
        if !locked.contains_key(name) {
            out.push(Drift {
                severity: Severity::Benign,
                target: Target::Bucket(name.clone()),
                what: "bucket added".into(),
                note: String::new(),
            });
        }
    }

    out.sort_by(|a, b| {
        a.severity
            .cmp(&b.severity)
            .then_with(|| a.target.cmp(&b.target))
            .then_with(|| a.what.cmp(&b.what))
    });
    out
}

fn fmt_mimes(m: &Option<Vec<String>>) -> String {
    match m {
        None => "any".to_string(),
        Some(v) if v.is_empty() => "any".to_string(),
        Some(v) => format!("[{}]", v.join(", ")),
    }
}

fn fmt_key(cols: &[String]) -> String {
    if cols.is_empty() {
        "(none)".to_string()
    } else {
        format!("({})", cols.join(", "))
    }
}

fn fk_target(fk: &ForeignKey) -> String {
    format!("{}{}", fk.references, fmt_key(&fk.ref_columns))
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;

    fn col(name: &str, class: TypeClass) -> Column {
        Column {
            name: name.into(),
            sql_type: class.label(),
            class,
            nullable: true,
            has_default: false,
            generated: false,
            identity: false,
        }
    }

    fn base() -> Schema {
        let users = Table {
            id: TableId::new("public", "users"),
            columns: vec![
                Column {
                    nullable: false,
                    ..col("id", TypeClass::Int { bits: 64 })
                },
                col("email", TypeClass::Text { max_len: Some(100) }),
                col("age", TypeClass::Int { bits: 32 }),
            ],
            primary_key: vec!["id".into()],
            unique: vec![vec!["email".into()]],
            foreign_keys: vec![],
        };
        let mut tables = IndexMap::new();
        tables.insert(users.id.clone(), users);
        Schema {
            default_schema: "public".into(),
            tables,
            enums: IndexMap::new(),
        }
    }

    fn users(s: &mut Schema) -> &mut Table {
        s.tables.get_mut(&TableId::new("public", "users")).unwrap()
    }

    /// Classify `base()` against a mutated copy.
    fn drift_from(mutate: impl FnOnce(&mut Schema)) -> Report {
        let locked = base();
        let mut live = base();
        mutate(&mut live);
        classify(&locked, &live)
    }

    fn only(report: &Report) -> &Drift {
        assert_eq!(
            report.drifts.len(),
            1,
            "expected one drift, got {:#?}",
            report.drifts
        );
        &report.drifts[0]
    }

    #[test]
    fn identical_schemas_produce_no_drift() {
        let r = classify(&base(), &base());
        assert!(r.is_empty());
        assert!(!r.has_breaking());
        assert_eq!(r.render(), "schema matches seedle.lock\n");
    }

    // -- breaking -----------------------------------------------------------

    #[test]
    fn a_dropped_table_needs_confirmation_rather_than_aborting() {
        // The load still works; it just stops carrying that table's rows.
        let r = drift_from(|s| {
            s.tables.shift_remove(&TableId::new("public", "users"));
        });
        assert_eq!(only(&r).severity, Severity::Confirm);
        assert!(only(&r).what.contains("table dropped"));
        assert!(!r.has_breaking());
        assert!(r.needs_confirmation());
    }

    #[test]
    fn a_dropped_column_needs_confirmation_rather_than_aborting() {
        // Its data is discarded, which is a judgement call, not an error.
        let r = drift_from(|s| {
            users(s).columns.retain(|c| c.name != "age");
        });
        assert_eq!(only(&r).severity, Severity::Confirm);
        assert_eq!(only(&r).target.column(), Some("age"));
        assert!(only(&r).note.contains("discarded"));
        assert!(!r.has_breaking());
    }

    #[test]
    fn narrowing_a_type_is_breaking() {
        let r = drift_from(|s| {
            users(s).columns[2].class = TypeClass::Int { bits: 16 };
        });
        assert_eq!(only(&r).severity, Severity::Breaking);
        assert_eq!(only(&r).what, "int32 -> int16");
    }

    #[test]
    fn changing_type_class_is_breaking() {
        let r = drift_from(|s| {
            users(s).columns[2].class = TypeClass::Date;
        });
        assert_eq!(only(&r).severity, Severity::Breaking);
    }

    #[test]
    fn shortening_a_varchar_is_breaking() {
        let r = drift_from(|s| {
            users(s).columns[1].class = TypeClass::Text { max_len: Some(20) };
        });
        assert_eq!(only(&r).severity, Severity::Breaking);
        assert_eq!(only(&r).what, "varchar(100) -> varchar(20)");
    }

    #[test]
    fn tightening_nullability_without_a_default_is_breaking() {
        let r = drift_from(|s| {
            users(s).columns[2].nullable = false;
        });
        assert_eq!(only(&r).severity, Severity::Breaking);
        assert_eq!(only(&r).what, "nullable -> NOT NULL");
    }

    #[test]
    fn tightening_nullability_with_a_default_warns_but_still_flags_the_risk() {
        let r = drift_from(|s| {
            let c = &mut users(s).columns[2];
            c.nullable = false;
            c.has_default = true;
        });
        assert_eq!(only(&r).severity, Severity::Benign);
        // The residual risk must still be stated: a default does not rescue a
        // row that writes an explicit null.
        assert!(
            only(&r).note.contains("explicit null"),
            "{:?}",
            only(&r).note
        );
    }

    #[test]
    fn adding_a_not_null_column_without_a_default_is_breaking() {
        let r = drift_from(|s| {
            users(s).columns.push(Column {
                nullable: false,
                ..col("phone", TypeClass::Text { max_len: None })
            });
        });
        assert_eq!(only(&r).severity, Severity::Breaking);
        assert!(only(&r).what.contains("NOT NULL with no default"));
    }

    #[test]
    fn primary_key_change_is_breaking() {
        let r = drift_from(|s| {
            users(s).primary_key = vec!["email".into()];
        });
        assert_eq!(only(&r).severity, Severity::Breaking);
        assert!(only(&r).what.contains("primary key"));
    }

    #[test]
    fn dropping_a_unique_constraint_is_breaking() {
        let r = drift_from(|s| {
            users(s).unique.clear();
        });
        assert_eq!(only(&r).severity, Severity::Breaking);
        assert!(only(&r).what.contains("unique constraint dropped"));
    }

    #[test]
    fn adding_a_foreign_key_is_breaking() {
        let r = drift_from(|s| {
            users(s).foreign_keys.push(ForeignKey {
                name: "fk".into(),
                columns: vec!["age".into()],
                references: TableId::new("public", "orgs"),
                ref_columns: vec!["id".into()],
                deferrable: false,
            });
        });
        assert_eq!(only(&r).severity, Severity::Breaking);
        assert!(only(&r).what.contains("foreign key added"));
    }

    #[test]
    fn retargeting_a_foreign_key_is_breaking_and_a_rename_is_not_drift() {
        let with_fk = |target: &str, name: &str| {
            let mut s = base();
            users(&mut s).foreign_keys.push(ForeignKey {
                name: name.into(),
                columns: vec!["age".into()],
                references: TableId::new("public", target),
                ref_columns: vec!["id".into()],
                deferrable: false,
            });
            s
        };

        // Same columns, different target: breaking.
        let r = classify(&with_fk("orgs", "a"), &with_fk("teams", "a"));
        assert_eq!(only(&r).severity, Severity::Breaking);
        assert!(only(&r).what.contains("retargeted"), "{}", only(&r).what);

        // Same columns and target, different constraint name: not drift at all.
        let r = classify(&with_fk("orgs", "old_name"), &with_fk("orgs", "new_name"));
        assert!(
            r.is_empty(),
            "a constraint rename is not a schema change: {:#?}",
            r.drifts
        );
    }

    #[test]
    fn removing_an_enum_label_is_breaking() {
        let mut locked = base();
        users(&mut locked).columns.push(col(
            "tier",
            TypeClass::Enum {
                name: "tier".into(),
            },
        ));
        locked.enums.insert(
            "tier".into(),
            vec!["free".into(), "pro".into(), "team".into()],
        );
        let mut live = locked.clone();
        live.enums
            .insert("tier".into(), vec!["free".into(), "pro".into()]);

        let r = classify(&locked, &live);
        assert_eq!(only(&r).severity, Severity::Breaking);
        assert!(
            only(&r).what.contains("\"team\" removed"),
            "{}",
            only(&r).what
        );
    }

    #[test]
    fn making_a_column_generated_is_breaking() {
        let r = drift_from(|s| {
            users(s).columns[2].generated = true;
        });
        assert_eq!(only(&r).severity, Severity::Breaking);
        assert!(only(&r).what.contains("generated"));
    }

    // -- benign -------------------------------------------------------------

    #[test]
    fn widening_an_integer_is_benign() {
        let r = drift_from(|s| {
            users(s).columns[2].class = TypeClass::Int { bits: 64 };
        });
        assert_eq!(only(&r).severity, Severity::Benign);
        assert!(!r.has_breaking());
    }

    #[test]
    fn widening_a_varchar_and_going_unbounded_are_benign() {
        let r = drift_from(|s| {
            users(s).columns[1].class = TypeClass::Text { max_len: Some(500) };
        });
        assert_eq!(only(&r).severity, Severity::Benign);

        let r = drift_from(|s| {
            users(s).columns[1].class = TypeClass::Text { max_len: None };
        });
        assert_eq!(only(&r).severity, Severity::Benign);
    }

    #[test]
    fn adding_a_nullable_column_is_benign() {
        let r = drift_from(|s| {
            users(s)
                .columns
                .push(col("phone", TypeClass::Text { max_len: None }));
        });
        assert_eq!(only(&r).severity, Severity::Benign);
    }

    #[test]
    fn adding_a_not_null_column_with_a_default_is_benign() {
        let r = drift_from(|s| {
            users(s).columns.push(Column {
                nullable: false,
                has_default: true,
                ..col("created_at", TypeClass::Timestamp { tz: true })
            });
        });
        assert_eq!(only(&r).severity, Severity::Benign);
    }

    #[test]
    fn relaxing_nullability_is_benign() {
        let r = drift_from(|s| {
            users(s).columns[0].nullable = true;
        });
        assert_eq!(only(&r).severity, Severity::Benign);
    }

    #[test]
    fn adding_a_table_is_benign() {
        let r = drift_from(|s| {
            let t = Table {
                id: TableId::new("public", "sessions"),
                columns: vec![col("id", TypeClass::Uuid)],
                primary_key: vec!["id".into()],
                unique: vec![],
                foreign_keys: vec![],
            };
            s.tables.insert(t.id.clone(), t);
        });
        assert_eq!(only(&r).severity, Severity::Benign);
        assert!(only(&r).what.contains("table added"));
    }

    #[test]
    fn adding_an_enum_label_is_benign() {
        let mut locked = base();
        users(&mut locked).columns.push(col(
            "tier",
            TypeClass::Enum {
                name: "tier".into(),
            },
        ));
        locked.enums.insert("tier".into(), vec!["free".into()]);
        let mut live = locked.clone();
        live.enums
            .insert("tier".into(), vec!["free".into(), "enterprise".into()]);

        let r = classify(&locked, &live);
        assert_eq!(only(&r).severity, Severity::Benign);
    }

    #[test]
    fn reordering_columns_is_not_drift() {
        // Columns are matched by name everywhere, so a physical reorder is a
        // non-event and must not block a load.
        let r = drift_from(|s| {
            users(s).columns.swap(1, 2);
        });
        assert!(
            r.is_empty(),
            "column order must not register as drift: {:#?}",
            r.drifts
        );
    }

    #[test]
    fn adding_a_unique_constraint_is_benign() {
        let r = drift_from(|s| {
            users(s).unique.push(vec!["age".into()]);
        });
        assert_eq!(only(&r).severity, Severity::Benign);
    }

    // -- reporting ----------------------------------------------------------

    #[test]
    fn report_groups_by_severity_worst_first_and_counts_each() {
        let r = drift_from(|s| {
            users(s).columns[2].class = TypeClass::Int { bits: 16 }; // breaking
            users(s).columns.retain(|c| c.name != "email"); // needs confirmation
            users(s)
                .columns
                .push(col("phone", TypeClass::Text { max_len: None })); // benign
        });
        assert_eq!(r.counts(), (1, 1, 1));
        assert!(r.has_breaking());
        assert!(r.needs_confirmation());

        let text = r.render();
        let b = text.find("breaking:").unwrap();
        let c = text.find("needs confirmation:").unwrap();
        let g = text.find("benign:").unwrap();
        assert!(b < c && c < g, "worst must come first:\n{text}");
        assert!(
            text.contains("1 breaking, 1 needing confirmation, 1 benign"),
            "{text}"
        );
        assert!(
            text.contains("seedle lock"),
            "the fix must be named:\n{text}"
        );
    }

    #[test]
    fn report_ordering_is_stable() {
        let r1 = drift_from(|s| {
            users(s).columns[2].class = TypeClass::Int { bits: 16 };
            users(s).columns[1].class = TypeClass::Text { max_len: Some(1) };
            users(s)
                .columns
                .push(col("phone", TypeClass::Text { max_len: None }));
        });
        let r2 = drift_from(|s| {
            users(s)
                .columns
                .push(col("phone", TypeClass::Text { max_len: None }));
            users(s).columns[1].class = TypeClass::Text { max_len: Some(1) };
            users(s).columns[2].class = TypeClass::Int { bits: 16 };
        });
        assert_eq!(r1.render(), r2.render());
    }

    #[test]
    fn benign_only_report_does_not_suggest_relocking_as_a_fix() {
        let r = drift_from(|s| {
            users(s).columns[2].class = TypeClass::Int { bits: 64 };
        });
        assert!(!r.has_breaking());
        assert!(!r.render().contains("Review, then"), "{}", r.render());
    }
}

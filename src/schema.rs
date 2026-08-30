//! Engine-neutral schema model.
//!
//! Both Postgres and MySQL introspection funnel into these types, and everything
//! downstream (lock file, drift classification, value codecs, SQL generation)
//! speaks only this vocabulary.

use std::fmt;
use std::str::FromStr;

use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Fully-qualified table identifier.
///
/// In Postgres `schema` is a real schema (`public`); in MySQL it is the database
/// name. Parsed from and rendered as `schema.name`, or bare `name` when the
/// schema is the connection default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TableId {
    pub schema: Option<String>,
    pub name: String,
}

impl TableId {
    pub fn new(schema: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            schema: Some(schema.into()),
            name: name.into(),
        }
    }

    pub fn bare(name: impl Into<String>) -> Self {
        Self {
            schema: None,
            name: name.into(),
        }
    }

    /// True when this id could refer to `other`, treating a missing schema on
    /// either side as "the default schema", which `default` names.
    pub fn matches(&self, other: &TableId, default: &str) -> bool {
        if self.name != other.name {
            return false;
        }
        let a = self.schema.as_deref().unwrap_or(default);
        let b = other.schema.as_deref().unwrap_or(default);
        a == b
    }

    /// Filename stem used for this table's seed file. Schema-qualified only when
    /// it is not the default schema, so the common case stays `users.jsonl`.
    pub fn file_stem(&self, default: &str) -> String {
        match &self.schema {
            Some(s) if s != default => format!("{s}.{}", self.name),
            _ => self.name.clone(),
        }
    }
}

impl fmt::Display for TableId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.schema {
            Some(s) => write!(f, "{s}.{}", self.name),
            None => write!(f, "{}", self.name),
        }
    }
}

impl FromStr for TableId {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.is_empty() {
            return Err("empty table name".into());
        }
        // Only split on the last dot so `schema.name` works but a dotted
        // unqualified name is not silently mangled.
        match s.rsplit_once('.') {
            Some((schema, name)) if !schema.is_empty() && !name.is_empty() => {
                Ok(Self::new(schema, name))
            }
            Some(_) => Err(format!("malformed table name: {s:?}")),
            None => Ok(Self::bare(s)),
        }
    }
}

impl Serialize for TableId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for TableId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Coarse classification of a column type.
///
/// Drives value encoding, widening rules, and whether the generated `SELECT`
/// needs a `::text` cast. Serialized flattened into [`Column`], so variant
/// fields must not collide with `Column`'s own: hence `type_name`, not `name`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TypeClass {
    Bool,
    Int {
        bits: u8,
    },
    Float {
        bits: u8,
    },
    /// `precision`/`scale` are `None` for unconstrained numerics.
    Decimal {
        precision: Option<u16>,
        scale: Option<i16>,
    },
    /// `max_len` is `None` for unbounded text.
    Text {
        max_len: Option<u32>,
    },
    Bytes,
    Uuid,
    Json {
        binary: bool,
    },
    Date,
    Time {
        tz: bool,
    },
    Timestamp {
        tz: bool,
    },
    Interval,
    Enum {
        #[serde(rename = "type_name")]
        name: String,
    },
    Array {
        of: Box<TypeClass>,
    },
    /// Anything we do not model. Carried through as text, losslessly, but with
    /// no widening rules and no structural validation.
    Other {
        #[serde(rename = "type_name")]
        name: String,
    },
}

impl TypeClass {
    /// True when a value written for `self` can be stored in `to` without loss
    /// or rejection. Used to split benign drift from breaking drift.
    pub fn widens_to(&self, to: &TypeClass) -> bool {
        use TypeClass::*;
        match (self, to) {
            _ if self == to => true,

            (Int { bits: a }, Int { bits: b }) => a <= b,
            (Float { bits: a }, Float { bits: b }) => a <= b,
            // Integers land in floats and decimals safely enough for seed data;
            // the reverse can truncate.
            (Int { .. }, Decimal { .. }) => true,
            (Int { bits }, Float { bits: fb }) => {
                // f32 holds int24 exactly, f64 holds int53.
                (*bits <= 24 && *fb >= 32) || (*bits <= 53 && *fb >= 64)
            }

            (
                Decimal {
                    precision: pa,
                    scale: sa,
                },
                Decimal {
                    precision: pb,
                    scale: sb,
                },
            ) => match (pa, pb) {
                (_, None) => true,
                (None, Some(_)) => false,
                (Some(pa), Some(pb)) => {
                    let sa = sa.unwrap_or(0);
                    let sb = sb.unwrap_or(0);
                    // Both the integral and fractional halves must still fit.
                    pb >= pa && sb >= sa && (*pb as i32 - sb as i32) >= (*pa as i32 - sa as i32)
                }
            },

            (Text { max_len: a }, Text { max_len: b }) => match (a, b) {
                (_, None) => true,
                (None, Some(_)) => false,
                (Some(a), Some(b)) => a <= b,
            },

            // Everything renders to text, so text is the universal widening target.
            (_, Text { max_len: None }) => true,

            // MySQL has no uuid type; `CHAR(36)` is the stand-in, and a
            // canonical uuid is always exactly 36 characters.
            (Uuid, Text { max_len: Some(n) }) => *n >= 36,

            // jsonb vs json is storage, not fidelity: same text either way.
            (Json { .. }, Json { .. }) => true,

            (Date, Timestamp { .. }) => true,
            (Timestamp { tz: false }, Timestamp { tz: true }) => true,

            (Array { of: a }, Array { of: b }) => a.widens_to(b),

            _ => false,
        }
    }

    /// True when the engine cannot hand us this type in a form we decode
    /// natively, so the `SELECT` must cast it to text.
    pub fn needs_text_cast(&self) -> bool {
        matches!(
            self,
            TypeClass::Enum { .. }
                | TypeClass::Array { .. }
                | TypeClass::Interval
                | TypeClass::Other { .. }
        )
    }

    /// Short human label, used in drift output.
    pub fn label(&self) -> String {
        use TypeClass::*;
        match self {
            Bool => "bool".into(),
            Int { bits } => format!("int{bits}"),
            Float { bits } => format!("float{bits}"),
            Decimal {
                precision: Some(p),
                scale: Some(s),
            } => format!("decimal({p},{s})"),
            Decimal { .. } => "decimal".into(),
            Text { max_len: Some(n) } => format!("varchar({n})"),
            Text { .. } => "text".into(),
            Bytes => "bytes".into(),
            Uuid => "uuid".into(),
            Json { binary: true } => "jsonb".into(),
            Json { .. } => "json".into(),
            Date => "date".into(),
            Time { tz: true } => "timetz".into(),
            Time { .. } => "time".into(),
            Timestamp { tz: true } => "timestamptz".into(),
            Timestamp { .. } => "timestamp".into(),
            Interval => "interval".into(),
            Enum { name } => format!("enum {name}"),
            Array { of } => format!("{}[]", of.label()),
            Other { name } => name.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    /// Type name exactly as the engine reports it, for display and for the lock.
    pub sql_type: String,
    #[serde(flatten)]
    pub class: TypeClass,
    pub nullable: bool,
    /// Whether the column has a DEFAULT, an identity, or is generated, i.e.
    /// whether the DB can fill it in when the seed files omit it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub has_default: bool,
    /// Generated/computed columns cannot be written to at all.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub generated: bool,
    /// Serial / identity / AUTO_INCREMENT, needs sequence fixup after a load.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub identity: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForeignKey {
    pub name: String,
    pub columns: Vec<String>,
    pub references: TableId,
    pub ref_columns: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deferrable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Table {
    /// Redundant with the map key wherever a table is stored, so it is omitted
    /// from serialized form and refilled by [`Schema::hydrate`].
    #[serde(skip)]
    pub id: TableId,
    pub columns: Vec<Column>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub primary_key: Vec<String>,
    /// Unique constraints/indexes, each an ordered column list. Candidate
    /// upsert keys when there is no primary key.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unique: Vec<Vec<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub foreign_keys: Vec<ForeignKey>,
}

impl Table {
    pub fn column(&self, name: &str) -> Option<&Column> {
        self.columns.iter().find(|c| c.name == name)
    }

    /// Columns GraineSQL is allowed to write. Generated columns are never writable.
    pub fn writable_columns(&self) -> impl Iterator<Item = &Column> {
        self.columns.iter().filter(|c| !c.generated)
    }

    /// The key to conflict-target for an upsert: the primary key, else the first
    /// unique constraint. `None` means upsert is impossible for this table.
    pub fn upsert_key(&self) -> Option<&[String]> {
        if !self.primary_key.is_empty() {
            return Some(&self.primary_key);
        }
        self.unique.first().map(|u| u.as_slice())
    }

    /// Identity/serial columns that need their sequence advanced after a load.
    pub fn identity_columns(&self) -> impl Iterator<Item = &Column> {
        self.columns.iter().filter(|c| c.identity)
    }
}

/// A full introspected schema, keyed by table id.
///
/// `IndexMap` rather than `HashMap` so iteration order is insertion order and
/// therefore reproducible; introspection inserts in sorted order.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Schema {
    /// The connection's default schema, `public` on Postgres, the database name
    /// on MySQL. Needed to decide when a table id must be qualified.
    pub default_schema: String,
    pub tables: IndexMap<TableId, Table>,
    /// Enum types and their labels, keyed by type name. Labels are part of the
    /// contract: removing one can make an existing seed value unloadable.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub enums: IndexMap<String, Vec<String>>,
}

impl Schema {
    /// Refill the `id` of every table from its map key, after deserializing.
    pub fn hydrate(&mut self) {
        for (id, table) in self.tables.iter_mut() {
            table.id = id.clone();
        }
    }

    pub fn get(&self, id: &TableId) -> Option<&Table> {
        self.tables.get(id).or_else(|| {
            // Tolerate a bare id against a qualified schema, and vice versa.
            self.tables
                .iter()
                .find(|(k, _)| k.matches(id, &self.default_schema))
                .map(|(_, v)| v)
        })
    }

    /// Move every table from schema `from` onto schema `to`.
    ///
    /// On MySQL the "schema" is the database name, so a seed exported from one
    /// database would otherwise look entirely absent in another. Only the
    /// default schema moves; anything explicitly elsewhere stays put.
    pub fn rebase(&mut self, from: &str, to: &str) {
        if from == to {
            return;
        }
        let moved: IndexMap<TableId, Table> = self
            .tables
            .iter()
            .map(|(id, table)| {
                let mut table = table.clone();
                let id = rebase_id(id, from, to);
                for fk in &mut table.foreign_keys {
                    fk.references = rebase_id(&fk.references, from, to);
                }
                table.id = id.clone();
                (id, table)
            })
            .collect();
        self.tables = moved;
        self.default_schema = to.to_string();
    }

    /// Resolve a possibly-unqualified id to the canonical id used as a key.
    pub fn resolve(&self, id: &TableId) -> Option<TableId> {
        if self.tables.contains_key(id) {
            return Some(id.clone());
        }
        self.tables
            .keys()
            .find(|k| k.matches(id, &self.default_schema))
            .cloned()
    }
}

/// One id moved from schema `from` to `to`, if it was there.
fn rebase_id(id: &TableId, from: &str, to: &str) -> TableId {
    match id.schema.as_deref() {
        Some(s) if s == from => TableId::new(to, &id.name),
        None => TableId::new(to, &id.name),
        _ => id.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_id_parses_qualified_and_bare() {
        assert_eq!("users".parse::<TableId>().unwrap(), TableId::bare("users"));
        assert_eq!(
            "public.users".parse::<TableId>().unwrap(),
            TableId::new("public", "users")
        );
        assert!("".parse::<TableId>().is_err());
        assert!(".users".parse::<TableId>().is_err());
        assert!("public.".parse::<TableId>().is_err());
    }

    #[test]
    fn table_id_matches_across_default_schema() {
        let bare = TableId::bare("users");
        let qualified = TableId::new("public", "users");
        assert!(bare.matches(&qualified, "public"));
        assert!(qualified.matches(&bare, "public"));
        assert!(!qualified.matches(&TableId::new("audit", "users"), "public"));
    }

    #[test]
    fn file_stem_only_qualifies_non_default_schemas() {
        assert_eq!(TableId::new("public", "users").file_stem("public"), "users");
        assert_eq!(
            TableId::new("audit", "users").file_stem("public"),
            "audit.users"
        );
        assert_eq!(TableId::bare("users").file_stem("public"), "users");
    }

    #[test]
    fn int_widening_is_directional() {
        let i32t = TypeClass::Int { bits: 32 };
        let i64t = TypeClass::Int { bits: 64 };
        assert!(i32t.widens_to(&i64t));
        assert!(!i64t.widens_to(&i32t));
    }

    #[test]
    fn text_widening_respects_length() {
        let short = TypeClass::Text { max_len: Some(20) };
        let long = TypeClass::Text { max_len: Some(200) };
        let unbounded = TypeClass::Text { max_len: None };
        assert!(short.widens_to(&long));
        assert!(!long.widens_to(&short));
        assert!(long.widens_to(&unbounded));
        assert!(!unbounded.widens_to(&long));
    }

    #[test]
    fn everything_widens_to_unbounded_text() {
        let text = TypeClass::Text { max_len: None };
        for c in [
            TypeClass::Bool,
            TypeClass::Uuid,
            TypeClass::Timestamp { tz: true },
            TypeClass::Json { binary: true },
        ] {
            assert!(c.widens_to(&text), "{} should widen to text", c.label());
        }
    }

    #[test]
    fn a_uuid_widens_only_into_text_wide_enough_to_hold_it() {
        let uuid = TypeClass::Uuid;
        assert!(uuid.widens_to(&TypeClass::Text { max_len: Some(36) }));
        assert!(uuid.widens_to(&TypeClass::Text { max_len: Some(64) }));
        assert!(!uuid.widens_to(&TypeClass::Text { max_len: Some(35) }));
        // Text is not a uuid: the reverse stays breaking.
        assert!(!TypeClass::Text { max_len: Some(36) }.widens_to(&uuid));
    }

    #[test]
    fn json_widens_in_both_directions_regardless_of_storage() {
        let jsonb = TypeClass::Json { binary: true };
        let json = TypeClass::Json { binary: false };
        assert!(jsonb.widens_to(&json));
        assert!(json.widens_to(&jsonb));
        // Still not interchangeable with anything else.
        assert!(!jsonb.widens_to(&TypeClass::Int { bits: 64 }));
    }

    #[test]
    fn decimal_widening_checks_both_halves() {
        let a = TypeClass::Decimal {
            precision: Some(10),
            scale: Some(2),
        };
        // More scale but same precision loses integral digits.
        let narrower_integral = TypeClass::Decimal {
            precision: Some(10),
            scale: Some(4),
        };
        let wider = TypeClass::Decimal {
            precision: Some(14),
            scale: Some(4),
        };
        assert!(!a.widens_to(&narrower_integral));
        assert!(a.widens_to(&wider));
        assert!(!wider.widens_to(&a));
    }

    #[test]
    fn timestamp_gains_tz_but_does_not_lose_it() {
        let naive = TypeClass::Timestamp { tz: false };
        let aware = TypeClass::Timestamp { tz: true };
        assert!(naive.widens_to(&aware));
        assert!(!aware.widens_to(&naive));
        assert!(TypeClass::Date.widens_to(&naive));
        assert!(!naive.widens_to(&TypeClass::Date));
    }

    /// Every class must survive a round trip while flattened into a `Column`,
    /// whose own field names it shares a namespace with.
    #[test]
    fn every_type_class_round_trips_flattened_into_a_column() {
        let classes = [
            TypeClass::Bool,
            TypeClass::Int { bits: 64 },
            TypeClass::Float { bits: 32 },
            TypeClass::Decimal {
                precision: Some(10),
                scale: Some(2),
            },
            TypeClass::Decimal {
                precision: None,
                scale: None,
            },
            TypeClass::Text { max_len: Some(50) },
            TypeClass::Text { max_len: None },
            TypeClass::Bytes,
            TypeClass::Uuid,
            TypeClass::Json { binary: true },
            TypeClass::Date,
            TypeClass::Time { tz: true },
            TypeClass::Timestamp { tz: false },
            TypeClass::Interval,
            TypeClass::Enum {
                name: "tier".into(),
            },
            TypeClass::Array {
                of: Box::new(TypeClass::Enum {
                    name: "tier".into(),
                }),
            },
            TypeClass::Other {
                name: "tsvector".into(),
            },
        ];
        for class in classes {
            let col = Column {
                // Deliberately different from any enum type name, so a collision
                // between the two shows up as a changed value.
                name: "the_column".into(),
                sql_type: "whatever".into(),
                class: class.clone(),
                nullable: true,
                has_default: true,
                generated: false,
                identity: false,
            };
            let yaml = serde_yaml_ng::to_string(&col).expect("serialize");
            let back: Column = serde_yaml_ng::from_str(&yaml)
                .unwrap_or_else(|e| panic!("{} failed to parse back: {e}\n{yaml}", class.label()));
            assert_eq!(back, col, "{} was mangled by:\n{yaml}", class.label());
            assert_eq!(
                back.name, "the_column",
                "the column name was overwritten:\n{yaml}"
            );
        }
    }

    #[test]
    fn upsert_key_prefers_primary_key() {
        let mut t = Table {
            id: TableId::bare("t"),
            columns: vec![],
            primary_key: vec!["id".into()],
            unique: vec![vec!["email".into()]],
            foreign_keys: vec![],
        };
        assert_eq!(t.upsert_key().unwrap(), ["id".to_string()]);
        t.primary_key.clear();
        assert_eq!(t.upsert_key().unwrap(), ["email".to_string()]);
        t.unique.clear();
        assert!(t.upsert_key().is_none());
    }
}

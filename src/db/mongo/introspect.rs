//! Collections as tables.
//!
//! A `$jsonSchema` validator is a contract the server enforces, so it becomes
//! real columns and every drift rule applies. An inferred profile is only what
//! some documents happened to contain, so it can never fail a load.

use anyhow::Result;
use bson::{Bson, Document};
use indexmap::IndexMap;

use super::{FieldProfile, class_for, classify, ops};
use crate::db::Db;
use crate::schema::{Column, Schema, Table, TableId, TypeClass, UniqueKey};

/// Stands in for a schema name; Mongo's equivalent is the database.
pub const SCHEMA: &str = "db";

pub fn database_name(db: &Db) -> Result<String> {
    ops::database_name(db)
}

pub async fn list_collections(db: &Db) -> Result<Vec<TableId>> {
    Ok(ops::list_collections(db)
        .await?
        .into_iter()
        .map(|c| TableId::new(SCHEMA, c))
        .collect())
}

/// Profiles for collections with no validator, keyed by collection.
pub type Profiles = IndexMap<String, Vec<FieldProfile>>;

pub async fn introspect(db: &Db) -> Result<Schema> {
    Ok(introspect_with_profiles(db).await?.0)
}

pub async fn introspect_with_profiles(db: &Db) -> Result<(Schema, Profiles)> {
    let mut tables: IndexMap<TableId, Table> = IndexMap::new();
    let mut enums: IndexMap<String, Vec<String>> = IndexMap::new();
    let mut profiles = Profiles::new();

    for name in ops::list_collections(db).await? {
        let id = TableId::new(SCHEMA, &name);
        let unique: Vec<UniqueKey> = ops::unique_indexes(db, &name)
            .await?
            .into_iter()
            .map(UniqueKey::total)
            .collect();

        let columns = match ops::validator(db, &name).await? {
            Some(schema) => from_validator(&name, &schema, &mut enums),
            None => {
                let docs = ops::find_sorted(db, &name, None, Some(ops::SAMPLE)).await?;
                let profile = profile_of(&docs);
                let cols = from_profile(&profile);
                profiles.insert(name.clone(), profile);
                cols
            }
        };

        tables.insert(
            id.clone(),
            Table {
                id,
                columns,
                primary_key: vec!["_id".into()],
                unique,
                // Mongo enforces no references, so there are none to record.
                foreign_keys: Vec::new(),
            },
        );
    }

    Ok((
        Schema {
            default_schema: SCHEMA.to_string(),
            tables,
            enums,
        },
        profiles,
    ))
}

/// Columns from a validator, which the server actually enforces.
fn from_validator(
    collection: &str,
    schema: &Document,
    enums: &mut IndexMap<String, Vec<String>>,
) -> Vec<Column> {
    let required: Vec<&str> = schema
        .get_array("required")
        .map(|r| r.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    let Ok(properties) = schema.get_document("properties") else {
        return vec![id_column()];
    };

    let mut columns = Vec::new();
    for (name, spec) in properties {
        let Some(spec) = spec.as_document() else {
            continue;
        };
        let types = bson_types(spec);
        let mut class = class_for(&types);

        // An `enum` keyword is a real constraint, so it is recorded like a
        // relational enum type. Keyed by collection and field, since Mongo has
        // no named types to borrow.
        if let Ok(values) = spec.get_array("enum") {
            let labels: Vec<String> = values
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
            if !labels.is_empty() {
                let key = format!("{collection}.{name}");
                enums.insert(key.clone(), labels);
                class = TypeClass::Enum { name: key };
            }
        }

        columns.push(Column {
            name: name.clone(),
            sql_type: types.join("|"),
            class,
            nullable: !required.contains(&name.as_str()),
            has_default: false,
            generated: false,
            identity: false,
        });
    }

    if !columns.iter().any(|c| c.name == "_id") {
        columns.insert(0, id_column());
    }
    columns.sort_by(|a, b| key_first(&a.name).cmp(&key_first(&b.name)));
    columns
}

/// Columns from an observed profile, which guarantees nothing.
///
/// Every field is nullable with a default, so no drift rule can make one
/// breaking: a field a sample did not happen to see is normal in Mongo.
fn from_profile(profile: &[FieldProfile]) -> Vec<Column> {
    let mut columns: Vec<Column> = profile
        .iter()
        .map(|f| Column {
            name: f.name.clone(),
            sql_type: f.bson_types.join("|"),
            class: class_for(&f.bson_types),
            nullable: true,
            has_default: true,
            generated: false,
            identity: false,
        })
        .collect();
    if !columns.iter().any(|c| c.name == "_id") {
        columns.insert(0, id_column());
    }
    columns.sort_by(|a, b| key_first(&a.name).cmp(&key_first(&b.name)));
    columns
}

/// `_id` sorts first; everything else alphabetically, so column order is stable.
fn key_first(name: &str) -> (u8, &str) {
    (u8::from(name != "_id"), name)
}

fn id_column() -> Column {
    Column {
        name: "_id".into(),
        sql_type: "objectId".into(),
        class: classify("objectId"),
        nullable: false,
        has_default: true,
        generated: false,
        identity: false,
    }
}

/// `bsonType` as a list, which the validator may give as a string or an array.
fn bson_types(spec: &Document) -> Vec<String> {
    match spec.get("bsonType") {
        Some(Bson::String(s)) => vec![s.clone()],
        Some(Bson::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        // `enum` without `bsonType` is a string enumeration in practice.
        _ if spec.contains_key("enum") => vec!["string".into()],
        _ => Vec::new(),
    }
}

/// What a sample of documents was observed to contain.
pub fn profile_of(docs: &[Document]) -> Vec<FieldProfile> {
    let sampled = docs.len() as u64;
    let mut seen: IndexMap<String, (u64, Vec<String>)> = IndexMap::new();

    for doc in docs {
        for (name, value) in doc {
            let entry = seen.entry(name.clone()).or_insert((0, Vec::new()));
            entry.0 += 1;
            let t = type_name(value).to_string();
            if !entry.1.contains(&t) {
                entry.1.push(t);
            }
        }
    }

    let mut out: Vec<FieldProfile> = seen
        .into_iter()
        .map(|(name, (present, mut bson_types))| {
            bson_types.sort();
            FieldProfile {
                name,
                bson_types,
                present,
                sampled,
            }
        })
        .collect();
    out.sort_by(|a, b| key_first(&a.name).cmp(&key_first(&b.name)));
    out
}

/// The `bsonType` keyword naming this value's type.
pub fn type_name(v: &Bson) -> &'static str {
    match v {
        Bson::Double(_) => "double",
        Bson::String(_) => "string",
        Bson::Array(_) => "array",
        Bson::Document(_) => "object",
        Bson::Boolean(_) => "bool",
        Bson::Null => "null",
        Bson::RegularExpression(_) => "regex",
        Bson::JavaScriptCode(_) | Bson::JavaScriptCodeWithScope(_) => "javascript",
        Bson::Int32(_) => "int",
        Bson::Int64(_) => "long",
        Bson::Timestamp(_) => "timestamp",
        Bson::Binary(b) if b.subtype == bson::spec::BinarySubtype::Uuid => "uuid",
        Bson::Binary(_) => "binData",
        Bson::ObjectId(_) => "objectId",
        Bson::DateTime(_) => "date",
        Bson::Symbol(_) => "symbol",
        Bson::Decimal128(_) => "decimal",
        Bson::Undefined => "undefined",
        Bson::MaxKey => "maxKey",
        Bson::MinKey => "minKey",
        Bson::DbPointer(_) => "dbPointer",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bson::doc;

    #[test]
    fn a_profile_records_presence_and_every_type_seen() {
        let docs = vec![
            doc! { "_id": 1, "name": "a", "score": 1 },
            doc! { "_id": 2, "name": "b" },
            doc! { "_id": 3, "name": "c", "score": "high" },
        ];
        let p = profile_of(&docs);
        let score = p.iter().find(|f| f.name == "score").unwrap();
        assert_eq!(score.present, 2);
        assert_eq!(score.sampled, 3);
        assert_eq!(score.bson_types, vec!["int", "string"]);
        assert!(score.polymorphic());
        assert!(!score.always_present());
    }

    #[test]
    fn profile_derived_columns_claim_nothing() {
        let docs = vec![doc! { "_id": 1, "name": "a" }];
        let cols = from_profile(&profile_of(&docs));
        for c in cols.iter().filter(|c| c.name != "_id") {
            assert!(c.nullable && c.has_default, "{} claims too much", c.name);
        }
    }

    #[test]
    fn a_validator_makes_required_fields_not_null() {
        let mut enums = IndexMap::new();
        let schema = doc! {
            "required": ["email"],
            "properties": {
                "email": { "bsonType": "string" },
                "nickname": { "bsonType": ["string", "null"] },
            }
        };
        let cols = from_validator("users", &schema, &mut enums);
        let email = cols.iter().find(|c| c.name == "email").unwrap();
        let nick = cols.iter().find(|c| c.name == "nickname").unwrap();
        assert!(!email.nullable);
        assert!(nick.nullable);
        // _id is always present, and sorts first.
        assert_eq!(cols[0].name, "_id");
    }

    #[test]
    fn a_validator_enum_becomes_a_real_enum_type() {
        let mut enums = IndexMap::new();
        let schema = doc! {
            "properties": { "plan": { "enum": ["free", "pro"] } }
        };
        let cols = from_validator("orgs", &schema, &mut enums);
        let plan = cols.iter().find(|c| c.name == "plan").unwrap();
        assert_eq!(
            plan.class,
            TypeClass::Enum {
                name: "orgs.plan".into()
            }
        );
        assert_eq!(enums["orgs.plan"], vec!["free", "pro"]);
    }
}

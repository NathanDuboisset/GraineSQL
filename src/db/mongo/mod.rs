//! MongoDB support.
//!
//! A collection is a table, a top-level field is a column, `_id` is the primary
//! key, and the database is the schema. There are no foreign keys, so load order
//! degenerates to alphabetical and the referential check has nothing to check.
//!
//! A [`$jsonSchema` validator][v] is a contract and gets real columns that every
//! drift rule applies to. A collection without one gets a [`FieldProfile`],
//! which is recorded but can never fail a load: in Mongo a new field is normal,
//! and treating an observation as a guarantee would make the lock lie.
//!
//! [v]: https://www.mongodb.com/docs/manual/core/schema-validation/

#[cfg(feature = "mongo")]
pub mod data;
#[cfg(feature = "mongo")]
pub mod introspect;
#[cfg(feature = "mongo")]
pub mod ops;
#[cfg(feature = "mongo")]
pub mod value;

#[cfg(feature = "mongo")]
pub use introspect::{database_name, introspect, list_collections};

use crate::schema::TypeClass;

/// What a collection's documents were observed to contain, from a sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldProfile {
    pub name: String,
    /// BSON type names seen, sorted. More than one means the field is
    /// polymorphic, which is legal and common.
    pub bson_types: Vec<String>,
    /// Documents in the sample that had the field at all.
    pub present: u64,
    pub sampled: u64,
}

impl FieldProfile {
    /// Never a guarantee: the sample may have missed a document without it.
    pub fn always_present(&self) -> bool {
        self.sampled > 0 && self.present == self.sampled
    }

    pub fn polymorphic(&self) -> bool {
        self.bson_types.len() > 1
    }
}

/// Map a BSON type name onto GraineSQL's classification.
pub fn classify(bson_type: &str) -> TypeClass {
    match bson_type {
        "bool" => TypeClass::Bool,
        "int" => TypeClass::Int { bits: 32 },
        "long" => TypeClass::Int { bits: 64 },
        "double" => TypeClass::Float { bits: 64 },
        // Decimal128 is exact, so it must not go through a float.
        "decimal" => TypeClass::Decimal {
            precision: Some(34),
            scale: None,
        },
        "string" | "javascript" | "symbol" => TypeClass::Text { max_len: None },
        "binData" => TypeClass::Bytes,
        // Not `Text`: the write path has to rebuild an ObjectId, and a string
        // `_id` is a different document, which would break every upsert.
        "objectId" => TypeClass::Other {
            name: "objectId".into(),
        },
        // A regex is not its source text; collapsing it would lose the flags.
        "regex" => TypeClass::Other {
            name: "regex".into(),
        },
        "uuid" => TypeClass::Uuid,
        // Milliseconds since the epoch in UTC: an instant, not a wall clock.
        "date" => TypeClass::Timestamp { tz: true },
        // A BSON Timestamp is an internal replication type, not an instant.
        "timestamp" => TypeClass::Other {
            name: "timestamp".into(),
        },
        "object" | "array" => TypeClass::Json { binary: true },
        "null" | "undefined" => TypeClass::Json { binary: true },
        other => TypeClass::Other {
            name: other.to_string(),
        },
    }
}

/// The class to record for a field, given every BSON type it was seen holding.
///
/// A field that is sometimes a string and sometimes a number has no narrower
/// class than "some JSON value".
pub fn class_for(bson_types: &[String]) -> TypeClass {
    // `null` alongside one real type just means the field is nullable.
    let mut real: Vec<&String> = bson_types
        .iter()
        .filter(|t| *t != "null" && *t != "undefined")
        .collect();
    real.sort();
    real.dedup();

    match real.as_slice() {
        [] => TypeClass::Json { binary: true },
        [only] => classify(only),
        _ => TypeClass::Json { binary: true },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_types_keep_their_width_and_exactness() {
        assert_eq!(classify("int"), TypeClass::Int { bits: 32 });
        assert_eq!(classify("long"), TypeClass::Int { bits: 64 });
        assert_eq!(classify("double"), TypeClass::Float { bits: 64 });
        assert!(matches!(classify("decimal"), TypeClass::Decimal { .. }));
    }

    #[test]
    fn a_bson_date_is_an_instant_but_a_timestamp_is_not() {
        assert_eq!(classify("date"), TypeClass::Timestamp { tz: true });
        assert!(matches!(classify("timestamp"), TypeClass::Other { .. }));
    }

    #[test]
    fn an_object_id_is_carried_verbatim_not_as_a_string() {
        assert!(matches!(classify("objectId"), TypeClass::Other { .. }));
        assert!(matches!(classify("regex"), TypeClass::Other { .. }));
    }

    #[test]
    fn nested_documents_are_carried_as_json() {
        assert_eq!(classify("object"), TypeClass::Json { binary: true });
        assert_eq!(classify("array"), TypeClass::Json { binary: true });
    }

    #[test]
    fn an_unmodelled_bson_type_is_carried_through() {
        assert_eq!(
            classify("dbPointer"),
            TypeClass::Other {
                name: "dbPointer".into()
            }
        );
    }

    #[test]
    fn a_nullable_field_keeps_its_underlying_class() {
        assert_eq!(
            class_for(&["string".into(), "null".into()]),
            TypeClass::Text { max_len: None }
        );
    }

    #[test]
    fn a_polymorphic_field_has_no_narrower_class_than_json() {
        assert_eq!(
            class_for(&["string".into(), "int".into()]),
            TypeClass::Json { binary: true }
        );
        assert_eq!(class_for(&[]), TypeClass::Json { binary: true });
    }

    #[test]
    fn presence_is_reported_without_being_claimed_as_a_constraint() {
        let seen_always = FieldProfile {
            name: "email".into(),
            bson_types: vec!["string".into()],
            present: 100,
            sampled: 100,
        };
        let sometimes = FieldProfile {
            name: "nickname".into(),
            bson_types: vec!["string".into(), "null".into()],
            present: 40,
            sampled: 100,
        };

        assert!(seen_always.always_present());
        assert!(!sometimes.always_present());
        assert!(!seen_always.polymorphic());
        assert!(sometimes.polymorphic());

        // An empty sample cannot claim a field is always present.
        let unsampled = FieldProfile {
            name: "x".into(),
            bson_types: vec![],
            present: 0,
            sampled: 0,
        };
        assert!(!unsampled.always_present());
    }
}

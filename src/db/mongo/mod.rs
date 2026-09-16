//! MongoDB support: the shape, and what still has to be decided.
//!
//! Nothing here talks to a server yet. What exists is the mapping from Mongo's
//! model onto GraineSQL's, worked out far enough to show where it fits and where it
//! does not, with the open questions written down rather than guessed at.
//!
//! # How it maps
//!
//! | GraineSQL | MongoDB |
//! |---|---|
//! | table | collection |
//! | column | field, at the top level of a document |
//! | primary key | `_id` |
//! | foreign key | nothing; references are a convention, not a constraint |
//! | schema (`public`) | database |
//! | enum type | a validator's `enum` keyword |
//!
//! Two consequences fall out of that:
//!
//! - **Load order does not exist.** There are no foreign keys to order by, so
//!   [`crate::order::topological`] degenerates to alphabetical, and the
//!   referential closure check has nothing to check. That is correct, not a
//!   limitation: Mongo genuinely does not enforce references.
//! - **The drift contract is weaker.** A relational lock records what the
//!   database *guarantees*. A collection with no validator guarantees nothing,
//!   so a lock can only record what was *observed*.
//!
//! # What counts as the schema when there is no schema
//!
//! Decided: a validator is a contract, an inferred profile is not.
//!
//! A collection may carry a [JSON Schema validator], in which case it is a real
//! contract and drift classification works exactly as it does for a table:
//! a removed property is breaking, a widened `bsonType` is benign, and so on.
//!
//! Most collections do not. For those the only available contract is an
//! inferred profile: which fields were seen, with which BSON types, in what
//! proportion of documents. [`FieldProfile`] is that. The question is what to do
//! with it, and it is a policy decision rather than a technical one:
//!
//! - Treating a profile as a contract makes `graine load` fail when a field the
//!   seed files never saw appears. That is wrong: in Mongo, a new field is
//!   normal and breaks nothing.
//! - Treating it as advisory makes the lock decorative for most collections,
//!   which undercuts the reason the lock exists.
//!
//! So profiles are recorded and reported as benign drift, and only validator
//! changes can be breaking. That keeps the guarantee honest, which matters more
//! than making every collection look locked.
//!
//! # What is genuinely hard
//!
//! BSON carries types JSON cannot: `ObjectId`, `Decimal128`, `Binary`, `Date`,
//! `Timestamp`, `Regex`, `Long`. Writing them as plain JSON would lose the
//! distinction between an `ObjectId` and the string that spells it, so the seed
//! files have to use [Extended JSON] (`{"$oid": "..."}`), and [`crate::value`]
//! needs to round-trip it. That is the real work, and it is the same fidelity
//! problem the relational side already solved: the tests to write are the same
//! shape as the ones in `value.rs`.
//!
//! [JSON Schema validator]: https://www.mongodb.com/docs/manual/core/schema-validation/
//! [Extended JSON]: https://www.mongodb.com/docs/manual/reference/mongodb-extended-json/

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

/// What a collection's documents were observed to contain.
///
/// Recorded per field, from a sample rather than a full scan, so it describes a
/// collection without claiming to constrain it.
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
    /// Whether every sampled document had this field.
    ///
    /// Never a guarantee: the sample may have missed a document without it.
    pub fn always_present(&self) -> bool {
        self.sampled > 0 && self.present == self.sampled
    }

    /// Whether the field held more than one BSON type.
    pub fn polymorphic(&self) -> bool {
        self.bson_types.len() > 1
    }
}

/// Map a BSON type name onto GraineSQL's classification.
///
/// Used for both validator `bsonType` keywords and observed types. A field with
/// several types has no single class, so callers pass each in turn and treat a
/// polymorphic field as [`TypeClass::Json`].
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
        // A BSON date is milliseconds since the epoch in UTC, so it is an
        // instant rather than a wall clock.
        "date" => TypeClass::Timestamp { tz: true },
        // A BSON Timestamp is an internal replication type, not an instant.
        "timestamp" => TypeClass::Other {
            name: "timestamp".into(),
        },
        // Nested documents and arrays are carried as JSON, the same way a
        // relational jsonb column is.
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
/// class than "some JSON value", and pretending otherwise would make the lock
/// claim a guarantee that does not hold.
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
        // Decimal128 must not become a float, for the same reason a SQL
        // numeric must not.
        assert!(matches!(classify("decimal"), TypeClass::Decimal { .. }));
    }

    #[test]
    fn a_bson_date_is_an_instant_but_a_timestamp_is_not() {
        assert_eq!(classify("date"), TypeClass::Timestamp { tz: true });
        // A BSON Timestamp is a replication counter, not a point in time.
        assert!(matches!(classify("timestamp"), TypeClass::Other { .. }));
    }

    #[test]
    fn an_object_id_is_carried_verbatim_not_as_a_string() {
        // As `Text` the write path would store a string `_id`, which is a
        // different document and breaks upsert.
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
        // null alongside one real type means nullable, not polymorphic.
        assert_eq!(
            class_for(&["string".into(), "null".into()]),
            TypeClass::Text { max_len: None }
        );
    }

    #[test]
    fn a_polymorphic_field_has_no_narrower_class_than_json() {
        // Claiming otherwise would make the lock assert a guarantee that does
        // not hold.
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

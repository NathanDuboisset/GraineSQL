//! BSON in and out of [`crate::value::Value`].
//!
//! No new `Value` or `TypeClass` variants: `Raw` already means "carried
//! verbatim, lossless because the write path rebuilds it from the same text",
//! and `Other` already means "modelled by nobody, no widening rules". That is
//! exactly what ObjectId, Timestamp and Regex need, so they use it, and nested
//! documents keep their Extended JSON inside a `Json`.

use anyhow::{Context, Result, bail};
use bson::{Binary, Bson, Document, spec::BinarySubtype};

use crate::schema::{Column, TypeClass};
use crate::value::Value;

/// One document as a row, in the schema's column order.
pub fn document_to_row(doc: &Document, columns: &[&Column]) -> Result<Vec<Value>> {
    columns
        .iter()
        .map(|col| match doc.get(&col.name) {
            None => Ok(Value::Null),
            Some(v) => bson_to_value(&col.class, v)
                .with_context(|| format!("reading field {:?}", col.name)),
        })
        .collect()
}

/// One row back into a document, dropping nulls so an absent field stays absent.
pub fn row_to_document(row: &[Value], columns: &[&Column]) -> Result<Document> {
    let mut doc = Document::new();
    for (col, value) in columns.iter().zip(row) {
        if value.is_null() {
            continue;
        }
        doc.insert(
            col.name.clone(),
            value_to_bson(&col.class, value)
                .with_context(|| format!("writing field {:?}", col.name))?,
        );
    }
    Ok(doc)
}

pub fn bson_to_value(class: &TypeClass, v: &Bson) -> Result<Value> {
    if matches!(v, Bson::Null | Bson::Undefined) {
        return Ok(Value::Null);
    }
    Ok(match (class, v) {
        (TypeClass::Bool, Bson::Boolean(b)) => Value::Bool(*b),
        (TypeClass::Int { .. }, Bson::Int32(i)) => Value::Int(*i as i64),
        (TypeClass::Int { .. }, Bson::Int64(i)) => Value::Int(*i),
        (TypeClass::Float { .. }, Bson::Double(f)) => Value::Float(*f),
        (TypeClass::Decimal { .. }, Bson::Decimal128(d)) => Value::Decimal(d.to_string()),
        (TypeClass::Text { .. }, Bson::String(s)) => Value::Text(s.clone()),
        // A validator `enum` constrains a string; it is not a nested document.
        (TypeClass::Enum { .. }, Bson::String(s)) => Value::Raw(s.clone()),
        (TypeClass::Bytes, Bson::Binary(b)) => Value::Bytes(b.bytes.clone()),
        (TypeClass::Uuid, Bson::Binary(b)) => Value::Uuid(
            uuid::Uuid::from_slice(&b.bytes)
                .context("a uuid-subtype binary that is not 16 bytes")?,
        ),
        (TypeClass::Timestamp { .. }, Bson::DateTime(d)) => Value::TimestampTz(
            chrono::DateTime::from_timestamp_millis(d.timestamp_millis())
                .context("a BSON date outside the range chrono can hold")?,
        ),
        // Verbatim forms: the write path rebuilds each from this exact text.
        (TypeClass::Other { name }, _) if name == "objectId" => match v {
            Bson::ObjectId(id) => Value::Raw(id.to_hex()),
            other => bail!(
                "expected an ObjectId, found {}",
                super::introspect::type_name(other)
            ),
        },
        (TypeClass::Other { name }, Bson::Timestamp(t)) if name == "timestamp" => {
            Value::Raw(format!("{}:{}", t.time, t.increment))
        }
        (TypeClass::Other { name }, Bson::RegularExpression(r)) if name == "regex" => {
            Value::Raw(format!("/{}/{}", r.pattern, r.options))
        }
        // Anything nested, or anything the class did not predict, keeps its
        // canonical Extended JSON so nothing is silently flattened.
        _ => Value::Json(
            serde_json::to_string(&v.clone().into_canonical_extjson())
                .context("rendering extended json")?,
        ),
    })
}

pub fn value_to_bson(class: &TypeClass, v: &Value) -> Result<Bson> {
    Ok(match v {
        Value::Null => Bson::Null,
        Value::Bool(b) => Bson::Boolean(*b),
        Value::Int(i) => match class {
            TypeClass::Int { bits: 32 } => Bson::Int32(
                i32::try_from(*i).with_context(|| format!("{i} does not fit a BSON int"))?,
            ),
            _ => Bson::Int64(*i),
        },
        Value::Float(f) => Bson::Double(*f),
        Value::Decimal(d) => Bson::Decimal128(
            d.parse()
                .map_err(|e| anyhow::anyhow!("{d:?} is not a Decimal128: {e:?}"))?,
        ),
        Value::Text(s) => Bson::String(s.clone()),
        Value::Bytes(b) => Bson::Binary(Binary {
            subtype: BinarySubtype::Generic,
            bytes: b.clone(),
        }),
        Value::Uuid(u) => Bson::Binary(Binary {
            subtype: BinarySubtype::Uuid,
            bytes: u.as_bytes().to_vec(),
        }),
        Value::TimestampTz(t) => Bson::DateTime(bson::DateTime::from_millis(t.timestamp_millis())),
        Value::Timestamp(t) => {
            Bson::DateTime(bson::DateTime::from_millis(t.and_utc().timestamp_millis()))
        }
        Value::Date(d) => Bson::DateTime(bson::DateTime::from_millis(
            d.and_hms_opt(0, 0, 0)
                .expect("midnight is a valid time")
                .and_utc()
                .timestamp_millis(),
        )),
        Value::Time(t) => Bson::String(t.to_string()),
        Value::Json(raw) => {
            let parsed: serde_json::Value =
                serde_json::from_str(raw).context("re-reading extended json")?;
            Bson::try_from(parsed).context("extended json that is not valid BSON")?
        }
        Value::Raw(text) => match class {
            TypeClass::Other { name } if name == "objectId" => Bson::ObjectId(
                text.parse()
                    .with_context(|| format!("{text:?} is not an ObjectId"))?,
            ),
            TypeClass::Other { name } if name == "timestamp" => {
                let (time, increment) = text
                    .split_once(':')
                    .context("a BSON timestamp is written as time:increment")?;
                Bson::Timestamp(bson::Timestamp {
                    time: time.parse().context("timestamp seconds")?,
                    increment: increment.parse().context("timestamp increment")?,
                })
            }
            TypeClass::Other { name } if name == "regex" => {
                let body = text.strip_prefix('/').unwrap_or(text);
                let (pattern, options) = body.rsplit_once('/').unwrap_or((body, ""));
                Bson::RegularExpression(bson::Regex {
                    pattern: pattern.to_string(),
                    options: options.to_string(),
                })
            }
            _ => Bson::String(text.clone()),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::mongo::classify;

    /// BSON -> Value -> BSON must be the identity, which is the same invariant
    /// `value.rs` enforces for the relational types.
    fn round_trip(bson_type: &str, v: Bson) {
        let class = classify(bson_type);
        let value = bson_to_value(&class, &v).expect("to value");
        let back = value_to_bson(&class, &value).expect("back to bson");
        assert_eq!(back, v, "{bson_type} did not round-trip through {value:?}");
    }

    #[test]
    fn every_bson_type_round_trips() {
        round_trip("bool", Bson::Boolean(true));
        round_trip("int", Bson::Int32(-7));
        round_trip("long", Bson::Int64(1 << 40));
        round_trip("double", Bson::Double(1.5));
        round_trip("string", Bson::String("hi".into()));
        round_trip(
            "binData",
            Bson::Binary(Binary {
                subtype: BinarySubtype::Generic,
                bytes: vec![0xDE, 0xAD, 0x00, 0xFF],
            }),
        );
        round_trip(
            "date",
            Bson::DateTime(bson::DateTime::from_millis(1_700_000_000_000)),
        );
    }

    #[test]
    fn an_object_id_survives_as_itself_not_as_a_string() {
        let id = bson::oid::ObjectId::parse_str("65a1b2c3d4e5f60718293a4b").unwrap();
        round_trip("objectId", Bson::ObjectId(id));

        // And it is written as bare hex, which is what makes the seed file
        // readable and the `_id` match on reload.
        let v = bson_to_value(&classify("objectId"), &Bson::ObjectId(id)).unwrap();
        assert_eq!(v, Value::Raw("65a1b2c3d4e5f60718293a4b".into()));
    }

    #[test]
    fn a_decimal_does_not_go_through_a_float() {
        let d: bson::Decimal128 = "19.9900".parse().unwrap();
        round_trip("decimal", Bson::Decimal128(d));
        let v = bson_to_value(&classify("decimal"), &Bson::Decimal128(d)).unwrap();
        assert_eq!(v, Value::Decimal("19.9900".into()));
    }

    #[test]
    fn a_validator_enum_is_a_plain_string_not_a_json_document() {
        let class = TypeClass::Enum {
            name: "orgs.plan".into(),
        };
        let v = bson_to_value(&class, &Bson::String("pro".into())).unwrap();
        assert_eq!(v, Value::Raw("pro".into()));
        assert_eq!(
            value_to_bson(&class, &v).unwrap(),
            Bson::String("pro".into())
        );
    }

    #[test]
    fn a_regex_keeps_its_flags() {
        round_trip(
            "regex",
            Bson::RegularExpression(bson::Regex {
                pattern: "^a.*z$".into(),
                options: "im".into(),
            }),
        );
    }

    #[test]
    fn a_replication_timestamp_is_not_confused_with_a_date() {
        round_trip(
            "timestamp",
            Bson::Timestamp(bson::Timestamp {
                time: 1_700_000_000,
                increment: 3,
            }),
        );
    }

    #[test]
    fn a_nested_document_keeps_its_bson_only_types() {
        let id = bson::oid::ObjectId::parse_str("65a1b2c3d4e5f60718293a4b").unwrap();
        let nested = Bson::Document(bson::doc! { "ref": id, "n": 1i64 });
        let class = classify("object");
        let v = bson_to_value(&class, &nested).unwrap();
        // Extended JSON, so the ObjectId is still an ObjectId in the file.
        match &v {
            Value::Json(raw) => assert!(raw.contains("$oid"), "{raw}"),
            other => panic!("expected json, got {other:?}"),
        }
        assert_eq!(value_to_bson(&class, &v).unwrap(), nested);
    }

    #[test]
    fn a_uuid_keeps_its_subtype() {
        let u = uuid::Uuid::parse_str("9f2c4b1e-7a3d-4e5f-8b9c-0d1e2f3a4b5c").unwrap();
        round_trip(
            "uuid",
            Bson::Binary(Binary {
                subtype: BinarySubtype::Uuid,
                bytes: u.as_bytes().to_vec(),
            }),
        );
    }

    #[test]
    fn an_absent_field_reads_as_null_and_is_not_written_back() {
        let col = Column {
            name: "missing".into(),
            sql_type: "string".into(),
            class: classify("string"),
            nullable: true,
            has_default: true,
            generated: false,
            identity: false,
        };
        let refs = vec![&col];
        let row = document_to_row(&bson::doc! { "other": 1 }, &refs).unwrap();
        assert_eq!(row, vec![Value::Null]);
        // Absent stays absent rather than becoming an explicit null.
        assert!(row_to_document(&row, &refs).unwrap().is_empty());
    }
}

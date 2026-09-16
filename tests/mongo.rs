//! MongoDB, exercised the same way the relational engines are.
//!
//! Runs only when `GRAINE_TEST_MONGO` names a server. A replica set is needed
//! for the transaction path; a standalone exercises the refusal instead.

#![cfg(feature = "mongo")]

mod common;

use std::process::Command;

use common::graine_bin;

fn base_url() -> Option<String> {
    std::env::var("GRAINE_TEST_MONGO")
        .ok()
        .filter(|u| !u.trim().is_empty())
}

macro_rules! require_mongo {
    () => {
        match base_url() {
            Some(u) => u,
            None => {
                eprintln!("skipping: GRAINE_TEST_MONGO is not set");
                return;
            }
        }
    };
}

/// A scratch directory wired to two throwaway databases on one server.
struct Mongo {
    dir: tempfile::TempDir,
    base: String,
    src: String,
    dst: String,
    rt: tokio::runtime::Runtime,
}

impl Mongo {
    fn new(name: &str, base: &str, tables: &str) -> Mongo {
        let m = Mongo {
            dir: tempfile::tempdir().expect("scratch dir"),
            base: base.trim_end_matches('/').to_string(),
            src: format!("graine_mg_{name}_src"),
            dst: format!("graine_mg_{name}_dst"),
            rt: tokio::runtime::Runtime::new().expect("tokio runtime"),
        };
        m.drop_databases();
        std::fs::write(
            m.dir.path().join("graine.yaml"),
            format!(
                "version: 1\n\
                 sources:\n  \
                   src:\n    engine: mongo\n    url: {}\n    default: true\n  \
                   dst:\n    engine: mongo\n    url: {}\n\
                 tables:\n{tables}",
                m.url(&m.src),
                m.url(&m.dst)
            ),
        )
        .unwrap();
        m
    }

    fn url(&self, db: &str) -> String {
        format!("{}/{db}", self.base)
    }

    fn client(&self) -> mongodb::Client {
        self.rt
            .block_on(mongodb::Client::with_uri_str(&self.base))
            .expect("connecting")
    }

    fn drop_databases(&self) {
        let client = self.client();
        for db in [&self.src, &self.dst] {
            let _ = self.rt.block_on(async { client.database(db).drop().await });
        }
    }

    /// Apply a `mongosh`-free setup closure against one database.
    fn with_db<F, T>(&self, db: &str, f: F) -> T
    where
        F: FnOnce(&tokio::runtime::Runtime, mongodb::Database) -> T,
    {
        let client = self.client();
        f(&self.rt, client.database(db))
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        Command::new(graine_bin())
            .args(args)
            .current_dir(self.dir.path())
            .output()
            .expect("running graine")
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "{args:?} failed\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    fn fail(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(!out.status.success(), "{args:?} unexpectedly succeeded");
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    }

    fn seed(&self, rel: &str) -> String {
        std::fs::read_to_string(self.dir.path().join("seed").join(rel))
            .unwrap_or_else(|e| panic!("reading seed/{rel}: {e}"))
    }
}

impl Drop for Mongo {
    fn drop(&mut self) {
        self.drop_databases();
    }
}

/// A validated collection and an unvalidated one, on both databases.
fn prepare(m: &Mongo, both: bool) {
    use bson::{Bson, doc};

    let dbs: Vec<String> = if both {
        vec![m.src.clone(), m.dst.clone()]
    } else {
        vec![m.src.clone()]
    };
    for name in dbs {
        m.with_db(&name, |rt, db| {
            rt.block_on(async {
                db.create_collection("users")
                    .validator(doc! { "$jsonSchema": {
                        "bsonType": "object",
                        "required": ["email", "plan"],
                        "properties": {
                            "email": { "bsonType": "string" },
                            "plan": { "enum": ["free", "pro", "team"] },
                            "nickname": { "bsonType": ["string", "null"] },
                        }
                    }})
                    .await
                    .unwrap();
                db.collection::<bson::Document>("users")
                    .create_index(
                        mongodb::IndexModel::builder()
                            .keys(doc! { "email": 1 })
                            .options(
                                mongodb::options::IndexOptions::builder()
                                    .unique(true)
                                    .build(),
                            )
                            .build(),
                    )
                    .await
                    .unwrap();
            });
        });
    }

    // Data only in the source, covering the BSON types JSON cannot carry.
    m.with_db(&m.src, |rt, db| {
        rt.block_on(async {
            db.collection("users")
                .insert_many(vec![
                    doc! { "_id": bson::oid::ObjectId::parse_str("65a1b2c3d4e5f60718293a4b").unwrap(),
                           "email": "a@x.test", "plan": "pro", "nickname": "Ann" },
                    doc! { "_id": bson::oid::ObjectId::parse_str("65a1b2c3d4e5f60718293a4c").unwrap(),
                           "email": "b@x.test", "plan": "free" },
                ])
                .await
                .unwrap();
            db.collection("events")
                .insert_many(vec![
                    doc! { "_id": 1, "kind": "click",
                           "at": bson::DateTime::from_millis(1_704_110_400_000),
                           "meta": doc! { "path": "/a", "n": 3i64 } },
                    doc! { "_id": 2, "kind": "view",
                           "score": Bson::Decimal128("19.9900".parse().unwrap()) },
                    doc! { "_id": 3, "kind": "click", "tags": ["a", "b"],
                           "blob": Bson::Binary(bson::Binary {
                               subtype: bson::spec::BinarySubtype::Generic,
                               bytes: vec![0xDE, 0xAD, 0xBE, 0xEF],
                           }) },
                ])
                .await
                .unwrap();
        });
    });
}

const TABLES: &str = "  users: {}\n  events: {}\n";

#[test]
fn a_collection_round_trips_through_seed_files() {
    let base = require_mongo!();
    let m = Mongo::new("roundtrip", &base, TABLES);
    prepare(&m, true);

    m.ok(&["lock", "-q"]);
    m.ok(&["export", "-q"]);
    m.ok(&["load", "--source", "dst", "--yes", "-q"]);
    m.ok(&["export", "--source", "dst", "-o", "back", "-q"]);

    for file in ["users.jsonl", "events.jsonl"] {
        let there = std::fs::read_to_string(m.dir.path().join("back").join(file)).unwrap();
        assert_eq!(m.seed(file), there, "{file} did not round-trip");
    }
}

#[test]
fn bson_only_types_survive_as_themselves() {
    let base = require_mongo!();
    let m = Mongo::new("bson", &base, TABLES);
    prepare(&m, true);
    m.ok(&["lock", "-q"]);
    m.ok(&["export", "-q"]);
    m.ok(&["load", "--source", "dst", "--yes", "-q"]);

    // Written readably rather than as extended JSON at the top level.
    let users = m.seed("users.jsonl");
    assert!(
        users.contains(r#""_id":"65a1b2c3d4e5f60718293a4b""#),
        "{users}"
    );
    assert!(
        users.contains(r#""plan":"pro""#),
        "a validator enum is a plain string:\n{users}"
    );
    let events = m.seed("events.jsonl");
    assert!(
        events.contains(r#""score":19.9900"#),
        "decimal kept its scale:\n{events}"
    );
    assert!(events.contains(r#""blob":"\\xdeadbeef""#), "{events}");
    // A nested document keeps its BSON-only types in extended JSON.
    assert!(events.contains(r#""$numberLong":"3""#), "{events}");

    // And they land as real BSON, not strings.
    m.with_db(&m.dst, |rt, db| {
        rt.block_on(async {
            let d = db
                .collection::<bson::Document>("events")
                .find_one(bson::doc! { "_id": 1 })
                .await
                .unwrap()
                .expect("event 1");
            assert!(matches!(d.get("at"), Some(bson::Bson::DateTime(_))));
            assert!(matches!(
                d.get_document("meta").unwrap().get("n"),
                Some(bson::Bson::Int64(3))
            ));
            let u = db
                .collection::<bson::Document>("users")
                .find_one(bson::doc! {})
                .await
                .unwrap()
                .expect("a user");
            assert!(matches!(u.get("_id"), Some(bson::Bson::ObjectId(_))));
        });
    });
}

#[test]
fn a_second_load_converges_rather_than_duplicating() {
    let base = require_mongo!();
    let m = Mongo::new("converge", &base, TABLES);
    prepare(&m, true);
    m.ok(&["lock", "-q"]);
    m.ok(&["export", "-q"]);

    for _ in 0..2 {
        m.ok(&["load", "--source", "dst", "--yes", "-q"]);
        m.with_db(&m.dst, |rt, db| {
            rt.block_on(async {
                let n = db
                    .collection::<bson::Document>("users")
                    .count_documents(bson::doc! {})
                    .await
                    .unwrap();
                assert_eq!(n, 2, "upsert duplicated documents");
            });
        });
    }
}

#[test]
fn a_collection_the_target_lacks_is_created_not_reported_as_dropped() {
    let base = require_mongo!();
    let m = Mongo::new("creates", &base, TABLES);
    // Only the source gets the validator, so `events` does not exist on dst.
    prepare(&m, false);
    m.ok(&["lock", "-q"]);
    m.ok(&["export", "-q"]);

    let out = m.run(&["load", "--source", "dst", "--yes"]);
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{all}");
    assert!(all.contains("a load creates it"), "{all}");
    assert!(!all.contains("will be discarded"), "{all}");
}

#[test]
fn a_where_clause_is_a_filter_document_not_sql() {
    let base = require_mongo!();
    let m = Mongo::new("filter", &base, "  events:\n    where: \"kind = click\"\n");
    prepare(&m, false);
    m.ok(&["lock", "-q"]);
    assert!(
        m.fail(&["export"]).contains("filter document"),
        "the error should say what a Mongo filter looks like"
    );

    let m = Mongo::new(
        "filter2",
        &base,
        "  events:\n    where: '{\"kind\": \"click\"}'\n",
    );
    prepare(&m, false);
    m.ok(&["lock", "-q"]);
    m.ok(&["export", "-q"]);
    assert_eq!(m.seed("events.jsonl").lines().count(), 2);
}

#[test]
fn sql_only_output_is_refused_rather_than_mangled() {
    let base = require_mongo!();
    let m = Mongo::new("sqlfmt", &base, "  events:\n    format: sql\n");
    assert!(
        m.fail(&["lock"]).contains("format: sql cannot be produced"),
        "a document engine has no dialect to generate SQL with"
    );
}

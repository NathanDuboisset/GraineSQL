//! Multi-row INSERT batching, which applies to every engine and every mode.
//!
//! The load it produces has to be indistinguishable from the one-statement-per-
//! row path, so most of these compare the two directly.

mod common;

use common::{DATA, Fixture, SCHEMA, TABLES};

fn prepared(name: &str, base: &str, batch: usize) -> Fixture {
    let f = Fixture::new(name, base, SCHEMA);
    f.sql_src(DATA).unwrap();
    f.write_config_with(&format!("load:\n  batch: {batch}\n"), TABLES);
    f.ok(&["lock", "-q"]);
    f.ok(&["export", "-q"]);
    f
}

#[test]
fn batched_and_per_row_loads_produce_the_same_data() {
    let base = require_pg!();
    for (name, batch) in [("batched", 500), ("single", 1)] {
        let f = prepared(name, &base, batch);
        f.ok(&["load", "--source", "dst", "--yes", "-q"]);
        // Re-exporting from the target reproduces the committed files exactly,
        // which is a stronger claim than matching row counts.
        f.ok(&["export", "--source", "dst", "-o", "out", "-q"]);
        if let Some(d) = common::dirs_differ(&f.seed_dir(), &f.path().join("out")) {
            panic!("batch={batch} changed the data:\n{d}");
        }
    }
}

#[test]
fn a_batched_upsert_still_converges() {
    let base = require_pg!();
    let f = prepared("batchupsert", &base, 500);
    for _ in 0..2 {
        f.ok(&["load", "--source", "dst", "--yes", "-q"]);
        assert_eq!(f.query_dst("SELECT count(*) FROM orgs"), "3");
        assert_eq!(f.query_dst("SELECT count(*) FROM users"), "4");
    }
}

#[test]
fn a_failing_row_is_named_even_inside_a_batch() {
    let base = require_pg!();
    let f = prepared("batcherr", &base, 500);
    // A CHECK constraint is invisible to the pre-flight validation, so the
    // failure can only surface mid-statement.
    f.sql_dst("ALTER TABLE orgs ADD CONSTRAINT no_globex CHECK (name <> 'Globex')")
        .unwrap();

    let e = f.fail(&["load", "--source", "dst", "--yes"]);
    e.says("inserting row");
    e.says("orgs");
}

#[test]
fn a_batch_that_exceeds_the_parameter_cap_is_split_not_rejected() {
    let base = require_pg!();
    // 60 columns at batch 5000 would be 300k parameters, well past Postgres's
    // 65535, so the batch has to shrink on its own.
    let cols: Vec<String> = (0..60).map(|i| format!("c{i} text")).collect();
    let f = Fixture::new(
        "batchwide",
        &base,
        &format!(
            "CREATE TABLE wide (id int PRIMARY KEY, {});",
            cols.join(", ")
        ),
    );
    let vals: Vec<String> = (0..60).map(|i| format!("'v{i}'")).collect();
    for i in 1..=50 {
        f.sql_src(&format!(
            "INSERT INTO wide VALUES ({i}, {})",
            vals.join(", ")
        ))
        .unwrap();
    }
    f.write_config_with("load:\n  batch: 5000\n", "  wide: {}\n");
    f.ok(&["lock", "-q"]);
    f.ok(&["export", "-q"]);
    f.ok(&["load", "--source", "dst", "--yes", "-q"]);
    assert_eq!(f.query_dst("SELECT count(*) FROM wide"), "50");
}

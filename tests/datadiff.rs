//! `graine diff --data`: which rows a load would add, change or remove.

mod common;

use common::{DATA, Fixture, SCHEMA, TABLES};

fn prepared(name: &str, base: &str) -> Fixture {
    let f = Fixture::new(name, base, SCHEMA);
    f.sql_src(DATA).unwrap();
    f.write_config(TABLES);
    f.ok(&["lock", "-q"]);
    f.ok(&["export", "-q"]);
    f
}

#[test]
fn an_untouched_target_reports_no_change() {
    let base = require_pg!();
    let f = prepared("diffsame", &base);
    f.ok(&["load", "--source", "dst", "--yes", "-q"]);

    let d = f.ok(&["diff", "--data", "--source", "dst"]);
    d.says("(no change)");
    d.does_not_say(" +");
    d.does_not_say(" ~");
}

#[test]
fn an_empty_target_reports_every_row_as_added() {
    let base = require_pg!();
    let f = prepared("diffadd", &base);

    let d = f.ok(&["diff", "--data", "--source", "dst"]);
    d.says("+3");
    d.says("public.orgs");
}

#[test]
fn a_changed_row_names_the_columns_that_moved() {
    let base = require_pg!();
    let f = prepared("diffupdate", &base);
    f.ok(&["load", "--source", "dst", "--yes", "-q"]);
    f.sql_dst("UPDATE orgs SET name = 'Renamed' WHERE id = 1")
        .unwrap();

    let d = f.ok(&["diff", "--data", "--source", "dst"]);
    d.says("~1");
    d.says("name Renamed ->");
}

#[test]
fn a_row_only_the_target_has_shows_under_truncate_first() {
    let base = require_pg!();
    let f = Fixture::new("diffremove", &base, SCHEMA);
    f.sql_src(DATA).unwrap();
    f.write_config(&TABLES.replace("countries: {}", "countries:\n    load: truncate_first"));
    f.ok(&["lock", "-q"]);
    f.ok(&["export", "-q"]);
    f.ok(&["load", "--source", "dst", "--yes", "-q"]);

    f.sql_dst("INSERT INTO countries (code, name) VALUES ('zz', 'Nowhere')")
        .unwrap();

    f.ok(&["diff", "--data", "--source", "dst"]).says("-1");
}

#[test]
fn the_json_form_carries_the_same_counts() {
    let base = require_pg!();
    let f = prepared("diffjson", &base);
    let v = f
        .ok(&["diff", "--data", "--json", "--source", "dst"])
        .json();

    let orgs = v["tables"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["table"] == "public.orgs")
        .expect("orgs is missing from the json");
    assert_eq!(orgs["added"], 3);
    assert_eq!(orgs["updated"], 0);
}

#[test]
fn a_dry_run_load_shows_what_it_would_do() {
    let base = require_pg!();
    let f = prepared("diffdry", &base);
    let r = f.ok(&["load", "--source", "dst", "--dry-run", "--yes"]);
    r.says("+3");
    // And having said so, changed nothing.
    assert_eq!(f.query_dst("SELECT count(*) FROM orgs"), "0");
}

#[test]
fn schema_drift_blocks_the_row_comparison() {
    let base = require_pg!();
    let f = prepared("diffdrift", &base);
    f.sql_dst("ALTER TABLE orgs ADD COLUMN region text NOT NULL")
        .unwrap();

    f.fail(&["diff", "--data", "--source", "dst"])
        .says("cannot be compared");
}

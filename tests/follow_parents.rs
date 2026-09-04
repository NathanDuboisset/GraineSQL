//! `--follow-parents`: repairing a slice a filter would otherwise orphan.

mod common;

use common::{DATA, Fixture, SCHEMA};

/// Only one user, whose org the filter on `orgs` excludes.
const NARROW: &str = "  orgs:\n    where: \"id = 99\"\n  users:\n    where: \"org_id = 1\"\n";

#[test]
fn without_the_flag_the_orphan_is_reported() {
    let base = require_pg!();
    let f = Fixture::new("fpoff", &base, SCHEMA);
    f.sql_src(DATA).unwrap();
    f.write_config(NARROW);
    f.ok(&["lock", "-q"]);

    let e = f.fail(&["export"]);
    e.says("not referentially complete");
    e.says("--follow-parents");
}

#[test]
fn the_flag_pulls_the_parent_row_in() {
    let base = require_pg!();
    let f = Fixture::new("fpon", &base, SCHEMA);
    f.sql_src(DATA).unwrap();
    f.write_config(NARROW);
    f.ok(&["lock", "-q"]);

    let r = f.ok(&["export", "--follow-parents"]);
    r.says("pulled in to satisfy a foreign key");

    // The org the filter excluded is now in the file, and the slice loads.
    let orgs = f.read_seed("orgs.jsonl");
    assert!(
        orgs.contains("\"id\":1"),
        "the parent row is missing:\n{orgs}"
    );
    f.ok(&["load", "--source", "dst", "--yes", "-q"]);
    assert_eq!(f.query_dst("SELECT count(*) FROM orgs"), "1");
}

#[test]
fn the_walk_reaches_a_grandparent() {
    let base = require_pg!();
    let f = Fixture::new("fpdeep", &base, SCHEMA);
    f.sql_src(DATA).unwrap();
    // orders -> users -> orgs, with both ancestors filtered out.
    f.write_config(
        "  orgs:\n    where: \"id = 99\"\n  \
         users:\n    where: \"1 = 0\"\n  \
         orders: {}\n",
    );
    f.ok(&["lock", "-q"]);
    f.ok(&["export", "--follow-parents", "-q"]);

    f.ok(&["load", "--source", "dst", "--yes", "-q"]);
    assert!(
        f.query_dst("SELECT count(*) FROM orgs")
            .parse::<i64>()
            .unwrap()
            > 0,
        "the grandparent was never pulled in"
    );
}

#[test]
fn a_limit_keeps_its_own_rows_and_gains_the_pulled_ones() {
    let base = require_pg!();
    let f = Fixture::new("fplimit", &base, SCHEMA);
    f.sql_src(DATA).unwrap();
    // One org by limit, but users reference more than that one.
    f.write_config("  orgs:\n    limit: 1\n  users: {}\n");
    f.ok(&["lock", "-q"]);
    f.ok(&["export", "--follow-parents", "-q"]);

    f.ok(&["load", "--source", "dst", "--yes", "-q"]);
    let orgs: i64 = f.query_dst("SELECT count(*) FROM orgs").parse().unwrap();
    let users: i64 = f.query_dst("SELECT count(*) FROM users").parse().unwrap();
    assert!(orgs > 1, "the limit cut the pulled rows back out: {orgs}");
    assert!(users > 0);
}

#[test]
fn output_stays_byte_identical_across_runs() {
    let base = require_pg!();
    let f = Fixture::new("fpdet", &base, SCHEMA);
    f.sql_src(DATA).unwrap();
    f.write_config(NARROW);
    f.ok(&["lock", "-q"]);

    f.ok(&["export", "--follow-parents", "-o", "a", "-q"]);
    f.ok(&["export", "--follow-parents", "-o", "b", "-q"]);
    if let Some(d) = common::dirs_differ(&f.path().join("a"), &f.path().join("b")) {
        panic!("pulled rows are not deterministic:\n{d}");
    }
}

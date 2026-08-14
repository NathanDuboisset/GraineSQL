//! Foreign-key cycles, and the ordering guarantees around them.

mod common;

use common::Fixture;

const CYCLE_SCHEMA: &str = "
CREATE TABLE a (id int PRIMARY KEY, b_id int);
CREATE TABLE b (id int PRIMARY KEY, a_id int);
";

/// Add the mutual foreign keys, deferrable or not.
fn add_cycle(f: &Fixture, deferrable: bool) {
    let suffix = if deferrable {
        "DEFERRABLE INITIALLY IMMEDIATE"
    } else {
        ""
    };
    for sql in [
        format!("ALTER TABLE a ADD CONSTRAINT a_b FOREIGN KEY (b_id) REFERENCES b(id) {suffix}"),
        format!("ALTER TABLE b ADD CONSTRAINT b_a FOREIGN KEY (a_id) REFERENCES a(id) {suffix}"),
    ] {
        f.sql_src(&sql).unwrap();
        f.sql_dst(&sql).unwrap();
    }
}

const CYCLE_TABLES: &str = "  a: {}\n  b: {}\n";

#[test]
fn a_non_deferrable_cycle_is_refused_with_the_fix_named() {
    let base = require_pg!();
    let f = Fixture::new("cycle_hard", &base, CYCLE_SCHEMA);
    f.sql_src("INSERT INTO a VALUES (1, NULL); INSERT INTO b VALUES (1, 1)")
        .unwrap();
    add_cycle(&f, false);
    f.write_config(CYCLE_TABLES);
    f.ok(&["lock", "-q"]);
    f.ok(&["export", "-q"]);

    let r = f.fail(&["load", "--source", "dst", "--yes"]);
    r.says("foreign-key cycle");
    r.says("DEFERRABLE");
    // Nothing may be left behind by the refusal.
    assert_eq!(f.query_dst("SELECT count(*) FROM a"), "0");
    assert_eq!(f.query_dst("SELECT count(*) FROM b"), "0");
}

#[test]
fn an_all_deferrable_cycle_loads_inside_one_transaction() {
    let base = require_pg!();
    let f = Fixture::new("cycle_soft", &base, CYCLE_SCHEMA);
    add_cycle(&f, true);
    f.sql_src(
        "BEGIN; SET CONSTRAINTS ALL DEFERRED; \
         INSERT INTO a VALUES (1, 1); INSERT INTO b VALUES (1, 1); COMMIT",
    )
    .unwrap();
    f.write_config(CYCLE_TABLES);
    f.ok(&["lock", "-q"]);
    f.ok(&["export", "-q"]);

    f.ok(&["load", "--source", "dst", "--yes"]);
    assert_eq!(f.query_dst("SELECT count(*) FROM a"), "1");
    assert_eq!(f.query_dst("SELECT count(*) FROM b"), "1");
    // Mutual references really are satisfied, not just present.
    assert_eq!(f.query_dst("SELECT b_id FROM a WHERE id = 1"), "1");
}

#[test]
fn a_self_reference_is_not_treated_as_a_cycle() {
    let base = require_pg!();
    let f = Fixture::new(
        "selfref",
        &base,
        "CREATE TABLE employees (
             id integer PRIMARY KEY,
             manager_id integer REFERENCES employees(id),
             name text NOT NULL
         );",
    );
    // A chain three deep, so ordering within the table matters.
    f.sql_src(
        "INSERT INTO employees VALUES (1, NULL, 'Boss'), (2, 1, 'Manager'), (3, 2, 'Worker')",
    )
    .unwrap();
    f.write_config("  employees: {}\n");
    f.ok(&["lock", "-q"]);
    f.ok(&["export", "-q"]);

    // No cycle complaint, and the rows land.
    f.ok(&["load", "--source", "dst", "--yes"])
        .does_not_say("cycle");
    assert_eq!(f.query_dst("SELECT count(*) FROM employees"), "3");
    assert_eq!(
        f.query_dst("SELECT name FROM employees WHERE manager_id = 2"),
        "Worker"
    );
}

#[test]
fn load_order_puts_parents_first() {
    let base = require_pg!();
    let f = Fixture::new(
        "order",
        &base,
        "CREATE TABLE top (id int PRIMARY KEY);
         CREATE TABLE mid (id int PRIMARY KEY, top_id int NOT NULL REFERENCES top(id));
         CREATE TABLE leaf (id int PRIMARY KEY, mid_id int NOT NULL REFERENCES mid(id));",
    );
    f.sql_src(
        "INSERT INTO top VALUES (1); INSERT INTO mid VALUES (1,1); INSERT INTO leaf VALUES (1,1)",
    )
    .unwrap();
    // Listed in reverse dependency order on purpose: the config order must not
    // decide the load order.
    f.write_config("  leaf: {}\n  mid: {}\n  top: {}\n");
    f.ok(&["lock", "-q"]);
    f.ok(&["export", "-q"]);

    let plan = f.ok(&["plan", "--source", "dst"]);
    let top = plan.stdout.find("top").expect("top in plan");
    let mid = plan.stdout.find("mid").expect("mid in plan");
    let leaf = plan.stdout.find("leaf").expect("leaf in plan");
    assert!(
        top < mid && mid < leaf,
        "load order ignores dependencies:\n{}",
        plan.stdout
    );

    // And the load itself works, which it could not if the order were wrong.
    f.ok(&["load", "--source", "dst", "--yes", "-q"]);
    assert_eq!(f.query_dst("SELECT count(*) FROM leaf"), "1");
}

#[test]
fn truncate_first_empties_children_before_parents() {
    let base = require_pg!();
    let f = Fixture::new(
        "revorder",
        &base,
        "CREATE TABLE top (id int PRIMARY KEY);
         CREATE TABLE leaf (id int PRIMARY KEY, top_id int NOT NULL REFERENCES top(id));",
    );
    f.sql_src("INSERT INTO top VALUES (1); INSERT INTO leaf VALUES (1,1)")
        .unwrap();
    f.write_config("  top:\n    load: truncate_first\n  leaf:\n    load: truncate_first\n");
    f.ok(&["lock", "-q"]);
    f.ok(&["export", "-q"]);

    // Pre-existing rows that the truncate has to clear. Deleting `top` before
    // `leaf` would violate the foreign key.
    f.sql_dst("INSERT INTO top VALUES (9); INSERT INTO leaf VALUES (9,9)")
        .unwrap();

    f.ok(&["load", "--source", "dst", "--yes", "-q"]);
    assert_eq!(f.query_dst("SELECT count(*) FROM top"), "1");
    assert_eq!(f.query_dst("SELECT id FROM top"), "1");
}

//! Schema drift policy, and the guarantee that a refused command changes
//! nothing.

mod common;

use common::{DATA, Fixture, SCHEMA, TABLES, dirs_differ};

/// A fixture with data exported and locked, ready for a schema change.
fn prepared(name: &str, base: &str) -> Fixture {
    let f = Fixture::new(name, base, SCHEMA);
    f.sql_src(DATA).unwrap();
    f.write_config(TABLES);
    f.ok(&["lock", "-q"]);
    f.ok(&["export", "-q"]);
    f
}

/// Snapshot the seed directory so a later comparison can prove nothing moved.
fn snapshot(f: &Fixture) -> std::path::PathBuf {
    let snap = f.path().join("snapshot");
    let mut cmd = std::process::Command::new("cp");
    cmd.args([
        "-r",
        &f.seed_dir().to_string_lossy(),
        &snap.to_string_lossy(),
    ]);
    cmd.status().expect("snapshotting the seed directory");
    snap
}

#[test]
fn an_unchanged_schema_reports_no_drift() {
    let base = require_pg!();
    let f = prepared("nodrift", &base);
    f.ok(&["diff"]).says("schema matches seedle.lock");
    f.ok(&["lock", "--check"]);
}

// ---------------------------------------------------------------------------
// breaking drift
// ---------------------------------------------------------------------------

/// Each of these must abort every data command, with the change named.
const BREAKING: &[(&str, &str, &str)] = &[
    (
        "narrowed type",
        "ALTER TABLE torture ALTER COLUMN t_i64 TYPE integer USING NULL",
        "int64 -> int32",
    ),
    (
        "shortened varchar",
        "ALTER TABLE users ALTER COLUMN display TYPE varchar(5) USING NULL",
        "varchar(5)",
    ),
    (
        "new NOT NULL column with no default",
        "ALTER TABLE countries ADD COLUMN region text NOT NULL DEFAULT 'x'; \
         ALTER TABLE countries ALTER COLUMN region DROP DEFAULT",
        "NOT NULL with no default",
    ),
    (
        "tightened nullability with no default",
        "UPDATE users SET display = 'x' WHERE display IS NULL; \
         ALTER TABLE users ALTER COLUMN display SET NOT NULL",
        "nullable -> NOT NULL",
    ),
    (
        "changed primary key",
        "ALTER TABLE countries DROP CONSTRAINT countries_pkey; \
         ALTER TABLE countries ADD PRIMARY KEY (name)",
        "primary key",
    ),
    (
        "dropped unique constraint",
        "ALTER TABLE orgs DROP CONSTRAINT orgs_name_key",
        "unique constraint dropped",
    ),
    (
        "new foreign key",
        // NOT VALID so the constraint is created without a backfill check;
        // it is still a foreign key, and still changes load order.
        "ALTER TABLE torture ADD CONSTRAINT t_fk FOREIGN KEY (t_i16) \
         REFERENCES employees(id) NOT VALID",
        "foreign key added",
    ),
    (
        "dropped foreign key",
        "ALTER TABLE orders DROP CONSTRAINT orders_user_id_fkey",
        "foreign key on (user_id) dropped",
    ),
    (
        "removed enum label",
        // Removing a label needs a full rebuild of the type in Postgres, and
        // any row still holding the doomed label has to be moved off it first.
        "ALTER TABLE orgs ALTER COLUMN plan DROP DEFAULT; \
         ALTER TABLE orgs ALTER COLUMN plan TYPE text; \
         ALTER TABLE torture ALTER COLUMN t_enum TYPE text; \
         UPDATE orgs SET plan = 'pro' WHERE plan = 'team'; \
         UPDATE torture SET t_enum = 'pro' WHERE t_enum = 'team'; \
         DROP TYPE tier; \
         CREATE TYPE tier AS ENUM ('free', 'pro'); \
         ALTER TABLE orgs ALTER COLUMN plan TYPE tier USING plan::tier; \
         ALTER TABLE torture ALTER COLUMN t_enum TYPE tier USING t_enum::tier",
        "removed",
    ),
    (
        "column became generated",
        "ALTER TABLE countries DROP COLUMN name; \
         ALTER TABLE countries ADD COLUMN name text GENERATED ALWAYS AS (code) STORED",
        "generated",
    ),
];

#[test]
fn breaking_drift_aborts_every_data_command() {
    let base = require_pg!();
    for (i, (label, sql, expected)) in BREAKING.iter().enumerate() {
        let f = prepared(&format!("break{i}"), &base);
        let before = snapshot(&f);
        // Applied to both databases: `export` judges the source against the
        // lock and `load` judges the target, so a change to only one of them
        // would leave the other command with nothing to complain about.
        f.sql_src(sql)
            .unwrap_or_else(|e| panic!("applying {label:?} to src: {e}"));
        f.sql_dst(sql)
            .unwrap_or_else(|e| panic!("applying {label:?} to dst: {e}"));

        // diff reports it and exits non-zero.
        let d = f.fail(&["diff"]);
        d.says("breaking");
        d.says(expected);

        // export refuses...
        let e = f.fail(&["export"]);
        e.says("schema drift");
        e.says("Nothing was changed");
        // ...and the committed files are untouched, which is the whole point.
        if let Some(diff) = dirs_differ(&before, &f.seed_dir()) {
            panic!("{label}: a refused export modified the seed files:\n{diff}");
        }

        // load refuses too.
        f.fail(&["load", "--source", "dst", "--yes"])
            .says("schema drift");
        assert_eq!(
            f.query_dst("SELECT count(*) FROM orgs"),
            "0",
            "{label}: a refused load wrote rows"
        );

        // lock --check is the CI form.
        f.fail(&["lock", "--check"]).says("out of date");
    }
}

/// Data loss rather than a failed load: the command asks instead of aborting.
const CONFIRM: &[(&str, &str, &str)] = &[
    (
        "dropped column",
        "ALTER TABLE employees DROP COLUMN name",
        "column dropped",
    ),
    ("dropped table", "DROP TABLE orders", "table dropped"),
];

#[test]
fn data_losing_drift_asks_rather_than_aborting() {
    let base = require_pg!();
    for (i, (label, sql, expected)) in CONFIRM.iter().enumerate() {
        let f = prepared(&format!("confirm{i}"), &base);
        f.sql_src(sql)
            .unwrap_or_else(|e| panic!("applying {label:?} to src: {e}"));
        f.sql_dst(sql)
            .unwrap_or_else(|e| panic!("applying {label:?} to dst: {e}"));

        let d = f.ok(&["diff"]);
        d.says("needs confirmation");
        d.says(expected);
        d.does_not_say("breaking:");

        // With no terminal to prompt on, the command refuses rather than
        // guessing which answer the user wanted.
        let refused = f.fail(&["export"]);
        refused.says("--yes");

        // And --yes accepts it.
        f.ok(&["export", "--yes", "-q"]);
    }
}

#[test]
fn force_proceeds_past_breaking_drift_but_says_so() {
    let base = require_pg!();
    let f = prepared("force", &base);
    for sql in [
        "ALTER TABLE users ADD COLUMN nickname text NOT NULL DEFAULT 'x'",
        "ALTER TABLE users ALTER COLUMN nickname DROP DEFAULT",
    ] {
        f.sql_src(sql).unwrap();
    }

    f.fail(&["export"]);
    f.ok(&["export", "--force"]).says("--force");
}

#[test]
fn relocking_accepts_the_change_and_unblocks_the_command() {
    let base = require_pg!();
    let f = prepared("relock", &base);
    f.sql_src("ALTER TABLE employees DROP COLUMN name").unwrap();

    f.fail(&["export"]).says("--yes");
    f.ok(&["lock"]).says("column dropped");
    f.ok(&["export", "-q"]);
    f.ok(&["diff"]).says("schema matches");
}

// ---------------------------------------------------------------------------
// benign drift
// ---------------------------------------------------------------------------

const BENIGN: &[(&str, &str, &str)] = &[
    (
        "widened integer",
        "ALTER TABLE torture ALTER COLUMN t_i16 TYPE bigint",
        "int16 -> int64",
    ),
    (
        "widened varchar",
        "ALTER TABLE orgs ALTER COLUMN name TYPE varchar(200)",
        "varchar(80) -> varchar(200)",
    ),
    (
        "varchar to unbounded text",
        "ALTER TABLE orgs ALTER COLUMN name TYPE text",
        "varchar(80) -> text",
    ),
    (
        "new nullable column",
        "ALTER TABLE users ADD COLUMN phone text",
        "column added",
    ),
    (
        "new NOT NULL column with a default",
        "ALTER TABLE users ADD COLUMN seen_at timestamptz NOT NULL DEFAULT now()",
        "NOT NULL with default",
    ),
    (
        "relaxed nullability",
        "ALTER TABLE countries ALTER COLUMN name DROP NOT NULL",
        "NOT NULL -> nullable",
    ),
    (
        "new enum label",
        "ALTER TYPE tier ADD VALUE 'enterprise'",
        "added",
    ),
    (
        "new unique constraint",
        "ALTER TABLE countries ADD CONSTRAINT c_name_uq UNIQUE (name)",
        "unique constraint added",
    ),
    (
        "unrelated new table",
        "CREATE TABLE sessions (id uuid PRIMARY KEY)",
        "",
    ),
];

#[test]
fn benign_drift_warns_and_lets_the_command_through() {
    let base = require_pg!();
    for (i, (label, sql, expected)) in BENIGN.iter().enumerate() {
        let f = prepared(&format!("benign{i}"), &base);
        // Both databases, so the re-export's updated lock still describes the
        // load target too.
        f.sql_src(sql)
            .unwrap_or_else(|e| panic!("applying {label:?} to src: {e}"));
        f.sql_dst(sql)
            .unwrap_or_else(|e| panic!("applying {label:?} to dst: {e}"));

        let d = f.ok(&["diff"]);
        d.does_not_say("breaking:");
        if !expected.is_empty() {
            d.says(expected);
        }

        // Both data commands proceed.
        f.ok(&["export"]);
        f.ok(&["load", "--source", "dst", "--yes", "-q"]);
        assert_eq!(
            f.query_dst("SELECT count(*) FROM orgs"),
            "3",
            "{label}: the load should have gone through"
        );
    }
}

#[test]
fn a_column_reorder_is_not_drift_at_all() {
    let base = require_pg!();
    let f = prepared("reorder", &base);
    // Drop and re-add moves the column to the end physically.
    f.sql_src("ALTER TABLE users DROP COLUMN display; ALTER TABLE users ADD COLUMN display text")
        .unwrap();
    f.sql_dst("ALTER TABLE users DROP COLUMN display; ALTER TABLE users ADD COLUMN display text")
        .unwrap();

    f.ok(&["diff"]).says("schema matches seedle.lock");
    f.ok(&["lock", "--check"]);
}

#[test]
fn a_constraint_rename_is_not_drift() {
    let base = require_pg!();
    let f = prepared("rename", &base);
    f.sql_src("ALTER TABLE orders RENAME CONSTRAINT orders_user_id_fkey TO orders_user_fk")
        .unwrap();
    f.ok(&["diff"]).says("schema matches seedle.lock");
}

// ---------------------------------------------------------------------------
// row-level checks the schema comparison cannot make
// ---------------------------------------------------------------------------

#[test]
fn a_null_in_a_now_not_null_column_is_caught_before_writing() {
    let base = require_pg!();
    let f = prepared("nullcheck", &base);

    // Add the column with a default so the drift is only benign, then let a seed
    // row carry an explicit null for it, which no schema comparison can see.
    f.sql_src("ALTER TABLE users ADD COLUMN nickname text")
        .unwrap();
    f.sql_dst("ALTER TABLE users ADD COLUMN nickname text")
        .unwrap();
    f.ok(&["lock", "-q"]);
    f.ok(&["export", "-q"]);

    f.sql_dst("ALTER TABLE users ALTER COLUMN nickname SET DEFAULT 'x'")
        .unwrap();
    f.sql_dst("ALTER TABLE users ALTER COLUMN nickname SET NOT NULL")
        .unwrap();

    let r = f.fail(&["load", "--source", "dst", "--yes"]);
    r.says("nickname");
    r.says("NOT NULL");
    assert_eq!(
        f.query_dst("SELECT count(*) FROM orgs"),
        "0",
        "the load must abort before writing anything"
    );
}

#[test]
fn a_load_failure_rolls_the_whole_thing_back() {
    let base = require_pg!();
    let f = prepared("rollback", &base);

    // Point the last table's last row at a parent that does not exist. Six
    // tables load successfully before it fails.
    let path = f.seed_dir().join("orders.jsonl");
    let text = std::fs::read_to_string(&path).unwrap();
    let broken: String = text
        .lines()
        .map(|l| {
            l.replace(
                "11111111-2222-3333-4444-555555555555",
                "00000000-0000-0000-0000-000000000000",
            )
        })
        .map(|l| format!("{l}\n"))
        .collect();
    std::fs::write(&path, broken).unwrap();

    let r = f.fail(&["load", "--source", "dst", "--yes"]);
    r.says("rolled back");
    r.says("orders");

    for table in [
        "orgs",
        "users",
        "countries",
        "employees",
        "torture",
        "templates",
    ] {
        assert_eq!(
            f.query_dst(&format!("SELECT count(*) FROM {table}")),
            "0",
            "{table} survived a rollback, so the load was not atomic"
        );
    }
}

#[test]
fn a_duplicate_key_in_a_seed_file_is_reported_not_silently_resolved() {
    let base = require_pg!();
    let f = prepared("dupkey", &base);

    let path = f.seed_dir().join("countries.jsonl");
    let first = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .to_string();
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, format!("{text}{first}\n")).unwrap();

    f.fail(&["load", "--source", "dst", "--yes"])
        .says("duplicates");
}

#[test]
fn verify_needs_no_database_and_catches_a_hand_edit() {
    let base = require_pg!();
    let f = prepared("verify", &base);
    f.ok(&["verify"]).says("verified 7 tables");

    // Break a value's type rather than its bytes, so the check has to actually
    // parse the file.
    let path = f.seed_dir().join("orders.jsonl");
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(
        &path,
        text.replace("\"amount\":19.9900", "\"amount\":\"not a number\""),
    )
    .unwrap();
    f.fail(&["verify"]).says("orders");
}

#[test]
fn verify_notices_a_row_count_that_no_longer_matches() {
    let base = require_pg!();
    let f = prepared("verifycount", &base);
    let path = f.seed_dir().join("countries.jsonl");
    let text = std::fs::read_to_string(&path).unwrap();
    let fewer: String = text.lines().skip(1).map(|l| format!("{l}\n")).collect();
    std::fs::write(&path, fewer).unwrap();
    f.fail(&["verify"]).says("recorded in seedle.lock");
}

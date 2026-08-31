//! Partial unique indexes as upsert conflict targets.
//!
//! `ON CONFLICT (cols)` does not match a partial index; the predicate has to be
//! repeated. Postgres rejects the statement outright when it is missing, so
//! these tests fail loudly rather than silently doing the wrong thing.

mod common;

use std::process::Command;

use common::Fixture;

/// One live row per `(tenant, slug)`, with soft-deleted rows exempt.
const SCHEMA: &str = r#"
CREATE TABLE docs (
    id         bigserial PRIMARY KEY,
    tenant     text NOT NULL,
    slug       text NOT NULL,
    title      text NOT NULL,
    deleted_at timestamptz
);
CREATE UNIQUE INDEX docs_live_slug ON docs (tenant, slug) WHERE deleted_at IS NULL;
"#;

const DATA: &str = "
INSERT INTO docs (tenant, slug, title, deleted_at) VALUES
  ('acme', 'intro', 'Intro', NULL),
  ('acme', 'intro', 'Old intro', '2024-01-01T00:00:00Z'),
  ('globex', 'intro', 'Their intro', NULL);
";

const TABLES: &str = "  docs:\n    key: [tenant, slug]\n";

#[test]
fn a_partial_unique_index_is_a_usable_conflict_target() {
    let base = require_pg!();
    let f = Fixture::new("partialkey", &base, SCHEMA);
    f.sql_src(DATA).unwrap();
    f.write_config(TABLES);
    f.ok(&["lock", "-q"]);
    f.ok(&["export", "-q"]);

    // The predicate has to reach the lock, or the conflict clause cannot
    // reproduce it.
    let lock = std::fs::read_to_string(f.seed_dir().join("graine.lock")).unwrap();
    assert!(
        lock.contains("predicate:") && lock.contains("deleted_at IS NULL"),
        "the lock did not record the partial predicate:\n{lock}"
    );

    // A total index over (tenant, slug) could not hold this data at all: two
    // rows share the pair and only the predicate keeps them legal.
    f.ok(&["load", "--source", "dst", "--yes", "-q"]);
    assert_eq!(f.query_dst("SELECT count(*) FROM docs"), "3");
}

#[test]
fn a_partial_target_converges_over_the_rows_it_arbitrates() {
    let base = require_pg!();
    let f = Fixture::new("partialconverge", &base, SCHEMA);
    f.sql_src(
        "INSERT INTO docs (tenant, slug, title) VALUES
           ('acme', 'intro', 'Intro'), ('globex', 'intro', 'Their intro');",
    )
    .unwrap();
    f.write_config(TABLES);
    f.ok(&["lock", "-q"]);
    f.ok(&["export", "-q"]);

    for _ in 0..2 {
        f.ok(&["load", "--source", "dst", "--yes", "-q"]);
        assert_eq!(f.query_dst("SELECT count(*) FROM docs"), "2");
    }
}

#[test]
fn a_partial_target_warns_that_rows_outside_it_do_not_upsert() {
    let base = require_pg!();
    let f = Fixture::new("partialwarn", &base, SCHEMA);
    f.sql_src(DATA).unwrap();
    f.write_config(TABLES);
    f.ok(&["lock", "-q"]);
    f.ok(&["export", "-q"]);

    f.ok(&["plan"]).says("is a partial unique index");
}

#[test]
fn the_conflict_clause_repeats_the_predicate() {
    let base = require_pg!();
    let f = Fixture::new("partialsql", &base, SCHEMA);
    f.sql_src(DATA).unwrap();
    f.write_config(&format!("{TABLES}    format: sql\n"));
    f.ok(&["lock", "-q"]);
    f.ok(&["export", "-q"]);

    let sql = f.read_seed("docs.sql");
    assert!(
        sql.contains("ON CONFLICT (\"tenant\", \"slug\") WHERE (deleted_at IS NULL)"),
        "the predicate is missing from the conflict clause:\n{sql}"
    );
}

#[test]
fn a_total_index_wins_over_a_partial_one_on_the_same_columns() {
    let base = require_pg!();
    let schema = format!("{SCHEMA}\nCREATE UNIQUE INDEX docs_slug ON docs (tenant, slug);");
    // Both indexes cover (tenant, slug); the total one covers every row, so it
    // is the target and no predicate is emitted. The data has to satisfy the
    // total index too, so no repeated slug here.
    let f = Fixture::new("partialboth", &base, &schema);
    f.sql_src(
        "INSERT INTO docs (tenant, slug, title, deleted_at) VALUES
           ('acme', 'intro', 'Intro', NULL),
           ('globex', 'intro', 'Their intro', NULL);",
    )
    .unwrap();
    f.write_config(&format!("{TABLES}    format: sql\n"));
    f.ok(&["lock", "-q"]);
    f.ok(&["export", "-q"]);

    let sql = f.read_seed("docs.sql");
    assert!(
        sql.contains("ON CONFLICT (\"tenant\", \"slug\") DO"),
        "expected the total index to be targeted:\n{sql}"
    );
    assert!(!sql.contains("WHERE deleted_at"), "{sql}");
}

#[test]
fn sqlite_recovers_the_predicate_from_the_stored_ddl() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("src.db");
    run_sqlite(
        &db,
        "CREATE TABLE docs (
             id INTEGER PRIMARY KEY,
             tenant TEXT NOT NULL,
             slug TEXT NOT NULL,
             title TEXT NOT NULL,
             deleted_at TEXT
         );
         CREATE UNIQUE INDEX docs_live_slug ON docs (tenant, slug) WHERE deleted_at IS NULL;
         INSERT INTO docs VALUES (1, 'acme', 'intro', 'Intro', NULL);",
    );

    std::fs::write(
        dir.path().join("graine.yaml"),
        "version: 1\n\
         sources:\n  src:\n    engine: sqlite\n    url: sqlite://src.db\n    default: true\n\
         tables:\n  docs:\n    key: [tenant, slug]\n    format: sql\n",
    )
    .unwrap();

    let out = Command::new(common::graine_bin())
        .args(["lock", "-q"])
        .current_dir(dir.path())
        .output()
        .expect("running graine");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let lock = std::fs::read_to_string(dir.path().join("seed/graine.lock")).unwrap();
    assert!(
        lock.contains("deleted_at IS NULL"),
        "the predicate was not recovered from sqlite_master:\n{lock}"
    );
}

fn run_sqlite(db: &std::path::Path, sql: &str) {
    let out = Command::new("python3")
        .arg("-c")
        .arg(format!(
            "import sqlite3,sys\nc=sqlite3.connect({:?})\nc.executescript(sys.stdin.read())\nc.commit()",
            db.to_string_lossy()
        ))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write;
            c.stdin.as_mut().unwrap().write_all(sql.as_bytes())?;
            c.wait_with_output()
        })
        .expect("running sqlite3");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

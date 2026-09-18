//! SQLite and MySQL, exercised the same way Postgres is.
//!
//! SQLite needs nothing installed. MySQL runs only when `GRAINE_TEST_MYSQL`
//! names a server, e.g. `mysql://root:pw@127.0.0.1:3306`.

mod common;

use std::process::Command;

use common::{dirs_differ, graine_bin};

const SQLITE_SCHEMA: &str = "
CREATE TABLE orgs (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL UNIQUE);
CREATE TABLE users (
  id      TEXT PRIMARY KEY,
  org_id  INTEGER NOT NULL REFERENCES orgs(id),
  email   VARCHAR(120) NOT NULL UNIQUE,
  display TEXT,
  prefs   JSON,
  active  BOOLEAN,
  score   REAL,
  amount  DECIMAL(14,4),
  data    BLOB
);
CREATE TABLE employees (
  id         INTEGER PRIMARY KEY,
  manager_id INTEGER REFERENCES employees(id),
  name       TEXT NOT NULL
);
";

const SQLITE_DATA: &str = "
INSERT INTO orgs (name) VALUES ('Acme'), ('Globex');
INSERT INTO users VALUES
  ('a1', 1, 'a@x.test', 'Ann', '{\"b\":1,\"a\":2}', 1, 1.5, '19.9900', X'DEADBEEF00FF'),
  ('b2', 2, 'b@x.test', NULL, NULL, 0, NULL, NULL, NULL);
INSERT INTO employees VALUES (1, NULL, 'Boss'), (2, 1, 'Worker');
";

/// A scratch directory holding two SQLite files and a config wiring both.
struct Sqlite {
    dir: tempfile::TempDir,
}

impl Sqlite {
    fn new(tables: &str) -> Sqlite {
        let s = Sqlite {
            dir: tempfile::tempdir().expect("scratch dir"),
        };
        for db in ["src.db", "dst.db"] {
            s.sql(db, SQLITE_SCHEMA);
        }
        s.sql("src.db", SQLITE_DATA);

        std::fs::write(
            s.dir.path().join("graine.yaml"),
            format!(
                "version: 1\n\
                 sources:\n  \
                   src:\n    engine: sqlite\n    url: sqlite://src.db\n    default: true\n  \
                   dst:\n    engine: sqlite\n    url: sqlite://dst.db\n\
                 tables:\n{tables}"
            ),
        )
        .unwrap();
        s
    }

    /// Run SQL through the `sqlite3` bindings the test process already has.
    fn sql(&self, db: &str, sql: &str) {
        let path = self.dir.path().join(db);
        let out = Command::new("python3")
            .arg("-c")
            .arg(format!(
                "import sqlite3,sys\nc=sqlite3.connect({:?})\nc.executescript(sys.stdin.read())\nc.commit()",
                path.to_string_lossy()
            ))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut ch| {
                use std::io::Write;
                ch.stdin.as_mut().unwrap().write_all(sql.as_bytes())?;
                ch.wait_with_output()
            })
            .expect("running sqlite");
        assert!(
            out.status.success(),
            "sqlite failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn query(&self, db: &str, sql: &str) -> String {
        let path = self.dir.path().join(db);
        let out = Command::new("python3")
            .arg("-c")
            .arg(format!(
                "import sqlite3\nc=sqlite3.connect({:?})\nprint('|'.join('' if v is None else str(v) for r in c.execute({:?}) for v in r))",
                path.to_string_lossy(),
                sql
            ))
            .output()
            .expect("running sqlite");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
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
            "expected success from {args:?}:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    }
}

#[test]
fn sqlite_round_trips_byte_identically() {
    let s = Sqlite::new("  orgs: {}\n  users: {}\n  employees: {}\n");
    s.ok(&["lock", "-q"]);
    s.ok(&["export", "-q"]);
    s.ok(&["export", "-o", "out-a", "-q"]);
    s.ok(&["load", "--source", "dst", "--yes", "-q"]);
    s.ok(&["export", "--source", "dst", "-o", "out-b", "-q"]);

    if let Some(d) = dirs_differ(&s.dir.path().join("out-a"), &s.dir.path().join("out-b")) {
        panic!("sqlite does not round-trip:\n{d}");
    }
}

#[test]
fn sqlite_preserves_awkward_values() {
    let s = Sqlite::new("  orgs: {}\n  users: {}\n  employees: {}\n");
    s.ok(&["lock", "-q"]);
    s.ok(&["export", "-q"]);
    s.ok(&["load", "--source", "dst", "--yes", "-q"]);

    // hex(NULL) is the empty string in SQLite, so a null blob would silently
    // become an empty one without the guard in the read expression.
    assert_eq!(
        s.query("dst.db", "SELECT data IS NULL FROM users WHERE id='b2'"),
        "1"
    );
    assert_eq!(
        s.query("dst.db", "SELECT hex(data) FROM users WHERE id='a1'"),
        "DEADBEEF00FF"
    );
    assert_eq!(
        s.query("dst.db", "SELECT active FROM users WHERE id='a1'"),
        "1"
    );
    assert_eq!(
        s.query("dst.db", "SELECT score FROM users WHERE id='a1'"),
        "1.5"
    );
    // A self-reference is resolved inside the table's own batch.
    assert_eq!(
        s.query("dst.db", "SELECT name FROM employees WHERE manager_id=1"),
        "Worker"
    );
}

#[test]
fn add_pulls_in_the_parents_a_table_needs() {
    // Adding `users` alone would produce a config that cannot load.
    let s = Sqlite::new("");
    std::fs::write(
        s.dir.path().join("graine.yaml"),
        "version: 1\n\
         sources:\n  dev: {engine: sqlite, url: \"sqlite://src.db\", default: true}\n\
         tables: {}\n",
    )
    .unwrap();

    let out = s.ok(&["add", "users"]);
    assert!(out.contains("orgs"), "orgs should come along:\n{out}");
    assert!(out.contains("parent"), "{out}");

    // `tables: {}` is a flow mapping, so the result has to still parse.
    s.ok(&["lock", "-q"]);
    s.ok(&["export", "-q"]);
    assert!(s.dir.path().join("seed/orgs.jsonl").exists());
    assert!(s.dir.path().join("seed/users.jsonl").exists());
}

#[test]
fn add_can_skip_the_parents() {
    let s = Sqlite::new("");
    std::fs::write(
        s.dir.path().join("graine.yaml"),
        "version: 1\n\
         sources:\n  dev: {engine: sqlite, url: \"sqlite://src.db\", default: true}\n\
         tables: {}\n",
    )
    .unwrap();

    s.ok(&["add", "users", "--no-parents"]);
    let cfg = std::fs::read_to_string(s.dir.path().join("graine.yaml")).unwrap();
    assert!(cfg.contains("users:"), "{cfg}");
    assert!(!cfg.contains("orgs:"), "{cfg}");
}

/// A config with an empty `tables:` block, ready for `add`.
fn empty_config(s: &Sqlite) {
    std::fs::write(
        s.dir.path().join("graine.yaml"),
        "version: 1\n\
         sources:\n  dev: {engine: sqlite, url: \"sqlite://src.db\", default: true}\n\
         tables: {}\n",
    )
    .unwrap();
}

#[test]
fn add_with_children_pulls_the_tables_that_hang_off_one() {
    let s = Sqlite::new("");
    empty_config(&s);

    let out = s.ok(&["add", "orgs", "--with-children"]);
    assert!(out.contains("children"), "{out}");
    let cfg = std::fs::read_to_string(s.dir.path().join("graine.yaml")).unwrap();
    assert!(cfg.contains("users:"), "users hangs off orgs:\n{cfg}");

    s.ok(&["lock", "-q"]);
    s.ok(&["export", "-q"]);
}

#[test]
fn add_depth_bounds_the_child_walk() {
    let s = Sqlite::new("");
    empty_config(&s);
    // employees references only itself, so depth cannot reach past users.
    s.ok(&["add", "orgs", "--with-children", "--depth", "1"]);
    let cfg = std::fs::read_to_string(s.dir.path().join("graine.yaml")).unwrap();
    assert!(cfg.contains("users:"), "{cfg}");
    assert!(!cfg.contains("employees:"), "{cfg}");
}

#[test]
fn add_reports_how_each_table_arrived() {
    let s = Sqlite::new("");
    empty_config(&s);
    let out = s.ok(&["add", "users", "--with-children", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).expect("json output");
    let added = v["added"].as_array().unwrap();
    let via = |t: &str| {
        added
            .iter()
            .find(|a| a["table"] == t)
            .map(|a| a["via"].as_str().unwrap().to_string())
    };
    assert_eq!(via("users").as_deref(), Some("named"));
    assert_eq!(via("orgs").as_deref(), Some("parent"));
}

#[test]
fn status_reports_whether_the_seed_is_current() {
    let s = Sqlite::new("  orgs: {}\n  users: {}\n  employees: {}\n");
    s.ok(&["lock", "-q"]);
    s.ok(&["export", "-q"]);

    let out = s.ok(&["status"]);
    assert!(out.contains("schema: matches"), "{out}");
    assert!(out.contains("in sync"), "{out}");

    // A row added behind GraineSQL's back shows as a count mismatch.
    s.sql("src.db", "INSERT INTO orgs (name) VALUES ('Later')");
    let out = s.ok(&["status"]);
    assert!(out.contains("seeded 2, live 3"), "{out}");
}

#[test]
fn plan_tree_shows_why_the_order_is_what_it_is() {
    let s = Sqlite::new("  orgs: {}\n  users: {}\n  employees: {}\n");
    s.ok(&["lock", "-q"]);
    s.ok(&["export", "-q"]);

    let out = s.ok(&["plan", "--tree"]);
    let orgs = out.find("main.orgs").expect("orgs in tree");
    let users = out.find("main.users").expect("users in tree");
    assert!(
        orgs < users,
        "a parent must be drawn above its child:\n{out}"
    );
    assert!(out.contains("`- main.users"), "{out}");
    // A self-reference is not a dependency between tables, so employees is a
    // root rather than nested under itself.
    assert!(out.contains("main.employees"), "{out}");
}

#[test]
fn sqlite_foreign_keys_are_enforced_during_a_load() {
    // They are off by default, so a load would otherwise accept orphans.
    let s = Sqlite::new("  users: {}\n");
    s.ok(&["lock", "-q"]);
    let out = s.run(&["export"]);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains("referentially complete"),
        "exporting users without orgs should be refused:\n{text}"
    );
}

macro_rules! require_mysql {
    () => {
        match std::env::var("GRAINE_TEST_MYSQL")
            .ok()
            .filter(|u| !u.trim().is_empty())
        {
            Some(url) => url,
            None => {
                eprintln!("skipping: set GRAINE_TEST_MYSQL to run the MySQL tests");
                return;
            }
        }
    };
}

const MYSQL_SCHEMA: &str = "
CREATE TABLE orgs (
  id   BIGINT AUTO_INCREMENT PRIMARY KEY,
  name VARCHAR(80) NOT NULL UNIQUE,
  plan ENUM('free','pro','team') NOT NULL DEFAULT 'free'
);
CREATE TABLE users (
  id          CHAR(36) PRIMARY KEY,
  org_id      BIGINT NOT NULL,
  email       VARCHAR(120) NOT NULL UNIQUE,
  display     TEXT,
  prefs       JSON,
  active      TINYINT(1),
  big         BIGINT UNSIGNED,
  amount      DECIMAL(14,4),
  data        BLOB,
  made        DATETIME,
  seen        TIMESTAMP NULL,
  email_lower VARCHAR(120) GENERATED ALWAYS AS (LOWER(email)) STORED,
  FOREIGN KEY (org_id) REFERENCES orgs(id)
);
CREATE TABLE employees (
  id INT PRIMARY KEY, manager_id INT NULL, name TEXT NOT NULL,
  FOREIGN KEY (manager_id) REFERENCES employees(id)
);
";

const MYSQL_DATA: &str = "
INSERT INTO orgs (name, plan) VALUES ('Acme','pro'), ('Globex','free');
INSERT INTO users (id,org_id,email,display,prefs,active,big,amount,data,made,seen) VALUES
 ('a1111111-1111-1111-1111-111111111111',1,'a@x.test','Ann','{\"b\":1,\"a\":2}',1,
  18446744073709551615,'19.9900',X'DEADBEEF00FF','2024-01-01 12:00:00','2024-01-01 12:00:00'),
 ('b2222222-2222-2222-2222-222222222222',2,'b@x.test',NULL,NULL,0,0,'-0.0001',NULL,NULL,NULL);
INSERT INTO employees VALUES (1,NULL,'Boss'),(2,1,'Worker');
";

struct Mysql {
    dir: tempfile::TempDir,
    base: String,
    src: String,
    dst: String,
    rt: tokio::runtime::Runtime,
}

impl Mysql {
    fn new(base: &str, name: &str) -> Mysql {
        let m = Mysql {
            dir: tempfile::tempdir().expect("scratch dir"),
            base: base.trim_end_matches('/').to_string(),
            src: format!("graine_{name}_src"),
            dst: format!("graine_{name}_dst"),
            rt: tokio::runtime::Runtime::new().expect("tokio runtime"),
        };
        for db in [m.src.clone(), m.dst.clone()] {
            m.admin(&format!("DROP DATABASE IF EXISTS {db}"));
            m.admin(&format!("CREATE DATABASE {db}"));
            m.sql(&db, MYSQL_SCHEMA);
        }
        m.sql(&m.src.clone(), MYSQL_DATA);

        std::fs::write(
            m.dir.path().join("graine.yaml"),
            format!(
                "version: 1\n\
                 sources:\n  \
                   src:\n    engine: mysql\n    url: {}/{}\n    default: true\n  \
                   dst:\n    engine: mysql\n    url: {}/{}\n\
                 tables:\n  orgs: {{}}\n  users: {{}}\n  employees: {{}}\n",
                m.base, m.src, m.base, m.dst
            ),
        )
        .unwrap();
        m
    }

    /// Run statements, splitting on `;` since the driver takes one at a time.
    fn exec(&self, url: &str, sql: &str) -> Result<(), String> {
        self.rt.block_on(async {
            use sqlx::Executor;
            let pool = sqlx::MySqlPool::connect(url)
                .await
                .map_err(|e| format!("connecting to {url}: {e}"))?;
            for stmt in sql.split(';') {
                if stmt.trim().is_empty() {
                    continue;
                }
                pool.execute(stmt)
                    .await
                    .map_err(|e| format!("running {}: {e}", stmt.trim()))?;
            }
            Ok(())
        })
    }

    fn admin(&self, sql: &str) {
        // `mysql` always exists and is a safe place to issue CREATE DATABASE.
        self.exec(&format!("{}/mysql", self.base), sql)
            .expect("mysql admin");
    }

    fn sql(&self, db: &str, sql: &str) {
        self.exec(&format!("{}/{db}", self.base), sql)
            .expect("mysql");
    }

    /// One scalar, as text, so assertions read the way the value looks.
    fn query(&self, db: &str, sql: &str) -> String {
        let url = format!("{}/{db}", self.base);
        self.rt
            .block_on(async {
                use sqlx::Row;
                let pool = sqlx::MySqlPool::connect(&url).await?;
                let row = sqlx::query(sql).fetch_one(&pool).await?;
                let v: Option<String> = row.try_get(0).or_else(|_| {
                    row.try_get::<Option<Vec<u8>>, _>(0)
                        .map(|b| b.map(|b| String::from_utf8_lossy(&b).to_string()))
                })?;
                Ok::<_, sqlx::Error>(v.unwrap_or_default())
            })
            .unwrap_or_else(|e| panic!("query {sql}: {e}"))
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = Command::new(graine_bin())
            .args(args)
            .current_dir(self.dir.path())
            .output()
            .expect("running graine");
        assert!(
            out.status.success(),
            "expected success from {args:?}:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    }
}

impl Drop for Mysql {
    fn drop(&mut self) {
        // Best effort: a leftover database only affects the next run, which
        // drops it anyway. Panicking here would abort the test process.
        for db in [self.src.clone(), self.dst.clone()] {
            let _ = self.exec(
                &format!("{}/mysql", self.base),
                &format!("DROP DATABASE IF EXISTS {db}"),
            );
        }
    }
}

#[test]
fn mysql_round_trips_byte_identically() {
    let base = require_mysql!();
    let m = Mysql::new(&base, "rt");
    m.ok(&["lock", "-q"]);
    m.ok(&["export", "-o", "out-a", "-q"]);
    m.ok(&["export", "-q"]);
    m.ok(&["load", "--source", "dst", "--yes", "-q"]);
    m.ok(&["export", "--source", "dst", "-o", "out-b", "-q"]);

    if let Some(d) = dirs_differ(&m.dir.path().join("out-a"), &m.dir.path().join("out-b")) {
        panic!("mysql does not round-trip:\n{d}");
    }
}

#[test]
fn mysql_type_mapping_matches_the_live_server() {
    let base = require_mysql!();
    let m = Mysql::new(&base, "types");
    m.ok(&["lock", "-q"]);
    m.ok(&["export", "-q"]);
    m.ok(&["load", "--source", "dst", "--yes", "-q"]);

    // The decisions that were guesses until a live server confirmed them.
    let checks: &[(&str, &str)] = &[
        // BIGINT UNSIGNED overruns i64, so it is carried as an exact decimal.
        (
            "SELECT CAST(big AS CHAR) FROM users WHERE id LIKE 'a1%'",
            "18446744073709551615",
        ),
        // TINYINT(1) read as a boolean writes back as 0/1.
        (
            "SELECT CAST(active AS CHAR) FROM users WHERE id LIKE 'a1%'",
            "1",
        ),
        (
            "SELECT CAST(amount AS CHAR) FROM users WHERE id LIKE 'a1%'",
            "19.9900",
        ),
        (
            "SELECT CAST(amount AS CHAR) FROM users WHERE id LIKE 'b2%'",
            "-0.0001",
        ),
        (
            "SELECT HEX(data) FROM users WHERE id LIKE 'a1%'",
            "DEADBEEF00FF",
        ),
        // DATETIME is a wall clock, TIMESTAMP is an instant; both come back.
        (
            "SELECT CAST(made AS CHAR) FROM users WHERE id LIKE 'a1%'",
            "2024-01-01 12:00:00",
        ),
        (
            "SELECT CAST(plan AS CHAR) FROM orgs WHERE name='Acme'",
            "pro",
        ),
        // A generated column is never written, but the server recomputes it.
        (
            "SELECT email_lower FROM users WHERE id LIKE 'a1%'",
            "a@x.test",
        ),
    ];
    for (sql, expected) in checks {
        assert_eq!(m.query(&m.dst.clone(), sql), *expected, "{sql}");
    }
}

#[test]
fn a_lock_is_portable_between_databases() {
    // MySQL's schema is the database name, so a lock taken from one database
    // has to check out against another or CI can never pass.
    let base = require_mysql!();
    let m = Mysql::new(&base, "portable");
    m.ok(&["lock", "-q"]);
    m.ok(&["export", "-q"]);
    m.ok(&["lock", "--check", "--source", "dst"]);
    m.ok(&["verify"]);
}

#[test]
fn mysql_sequences_are_advanced_past_the_loaded_rows() {
    let base = require_mysql!();
    let m = Mysql::new(&base, "seq");
    m.ok(&["lock", "-q"]);
    m.ok(&["export", "-q"]);
    m.ok(&["load", "--source", "dst", "--yes", "-q"]);

    m.sql(&m.dst.clone(), "INSERT INTO orgs (name) VALUES ('Fresh')");
    assert_eq!(
        m.query(
            &m.dst.clone(),
            "SELECT CAST(id AS CHAR) FROM orgs WHERE name='Fresh'"
        ),
        "3",
        "the next insert must not collide with a seeded id"
    );
}

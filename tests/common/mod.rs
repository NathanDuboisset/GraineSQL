//! Shared harness for the integration tests.
//!
//! Every test needs a real database; there is no useful way to fake schema
//! introspection. Set `SEEDLE_TEST_PG` to a Postgres URL with rights to create
//! databases. Without it the integration tests skip rather than fail, so
//! `cargo test` still works on a machine with no server.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// Base URL for the test server, or `None` to skip.
pub fn pg_base_url() -> Option<String> {
    std::env::var("SEEDLE_TEST_PG")
        .ok()
        .filter(|u| !u.trim().is_empty())
}

/// Skip the calling test unless a test server is configured.
#[macro_export]
macro_rules! require_pg {
    () => {
        match $crate::common::pg_base_url() {
            Some(url) => url,
            None => {
                eprintln!(
                    "skipping: set SEEDLE_TEST_PG to a Postgres URL to run the integration tests"
                );
                return;
            }
        }
    };
}

/// A scratch project directory with two throwaway databases.
pub struct Fixture {
    pub dir: tempfile::TempDir,
    pub base_url: String,
    pub src_db: String,
    pub dst_db: String,
}

impl Fixture {
    /// Create `<name>_src` and `<name>_dst`, both with `schema_sql` applied.
    pub fn new(name: &str, base_url: &str, schema_sql: &str) -> Fixture {
        let src_db = format!("seedle_{name}_src");
        let dst_db = format!("seedle_{name}_dst");

        for db in [&src_db, &dst_db] {
            psql(
                &admin_url(base_url),
                &format!("DROP DATABASE IF EXISTS {db}"),
            )
            .expect("dropping a leftover test database");
            psql(&admin_url(base_url), &format!("CREATE DATABASE {db}"))
                .expect("creating the test database");
        }

        let f = Fixture {
            dir: tempfile::tempdir().expect("scratch dir"),
            base_url: base_url.to_string(),
            src_db,
            dst_db,
        };
        f.sql_src(schema_sql).expect("applying the schema to src");
        f.sql_dst(schema_sql).expect("applying the schema to dst");
        f
    }

    pub fn src_url(&self) -> String {
        format!("{}/{}", self.base_url.trim_end_matches('/'), self.src_db)
    }

    pub fn dst_url(&self) -> String {
        format!("{}/{}", self.base_url.trim_end_matches('/'), self.dst_db)
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn sql_src(&self, sql: &str) -> Result<String, String> {
        psql(&self.src_url(), sql)
    }

    pub fn sql_dst(&self, sql: &str) -> Result<String, String> {
        psql(&self.dst_url(), sql)
    }

    /// One scalar value from `src`.
    pub fn query_src(&self, sql: &str) -> String {
        psql(&self.src_url(), sql)
            .expect("query")
            .trim()
            .to_string()
    }

    pub fn query_dst(&self, sql: &str) -> String {
        psql(&self.dst_url(), sql)
            .expect("query")
            .trim()
            .to_string()
    }

    /// Write the project's config, wiring both databases as sources.
    pub fn write_config(&self, tables_yaml: &str) {
        self.write_config_with("", tables_yaml);
    }

    /// Same, with extra top-level config sections.
    pub fn write_config_with(&self, extra: &str, tables_yaml: &str) {
        std::fs::write(
            self.path().join(".env.src"),
            format!("DATABASE_URL={}\n", self.src_url()),
        )
        .unwrap();
        std::fs::write(
            self.path().join(".env.dst"),
            format!("DATABASE_URL={}\n", self.dst_url()),
        )
        .unwrap();
        std::fs::write(
            self.path().join("seedle.yaml"),
            format!(
                "version: 1\n\
                 sources:\n  \
                   src:\n    engine: postgres\n    env_file: .env.src\n    default: true\n  \
                   dst:\n    engine: postgres\n    env_file: .env.dst\n\
                 {extra}\
                 tables:\n{tables_yaml}"
            ),
        )
        .unwrap();
    }

    /// Run seedle in the fixture directory.
    pub fn run(&self, args: &[&str]) -> Run {
        let out = Command::new(seedle_bin())
            .args(args)
            .current_dir(self.path())
            .output()
            .expect("running seedle");
        Run {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).to_string(),
            args: args.iter().map(|a| a.to_string()).collect(),
        }
    }

    /// Run seedle and require success.
    pub fn ok(&self, args: &[&str]) -> Run {
        let r = self.run(args);
        assert!(
            r.code == 0,
            "expected success from {:?}:\n{}",
            args,
            r.all()
        );
        r
    }

    /// Run seedle and require failure.
    pub fn fail(&self, args: &[&str]) -> Run {
        let r = self.run(args);
        assert!(
            r.code != 0,
            "expected failure from {:?}:\n{}",
            args,
            r.all()
        );
        r
    }

    pub fn seed_dir(&self) -> PathBuf {
        self.path().join("seed")
    }

    pub fn read_seed(&self, rel: &str) -> String {
        let p = self.seed_dir().join(rel);
        std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("reading {}: {e}", p.display()))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Best effort: a leftover database would only break the next run, which
        // drops it anyway.
        for db in [&self.src_db, &self.dst_db] {
            let _ = psql(
                &admin_url(&self.base_url),
                &format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)"),
            );
        }
    }
}

pub struct Run {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
    pub args: Vec<String>,
}

impl Run {
    /// Both streams, for assertion messages.
    pub fn all(&self) -> String {
        format!(
            "$ seedle {}\n[exit {}]\n--- stdout ---\n{}--- stderr ---\n{}",
            self.args.join(" "),
            self.code,
            self.stdout,
            self.stderr
        )
    }

    /// Assert that some output mentions `needle`.
    pub fn says(&self, needle: &str) -> &Self {
        assert!(
            self.stdout.contains(needle) || self.stderr.contains(needle),
            "expected output to mention {needle:?}:\n{}",
            self.all()
        );
        self
    }

    pub fn does_not_say(&self, needle: &str) -> &Self {
        assert!(
            !self.stdout.contains(needle) && !self.stderr.contains(needle),
            "expected output NOT to mention {needle:?}:\n{}",
            self.all()
        );
        self
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.stdout)
            .unwrap_or_else(|e| panic!("stdout is not json ({e}):\n{}", self.all()))
    }
}

fn admin_url(base: &str) -> String {
    format!("{}/postgres", base.trim_end_matches('/'))
}

fn psql(url: &str, sql: &str) -> Result<String, String> {
    let out = Command::new("psql")
        // The same session settings seedle pins on its own connections. Without
        // these, psql renders a timestamptz in the machine's local zone and an
        // interval in its default style, so assertions on canonical text would
        // pass or fail depending on where the test runs.
        .env(
            "PGOPTIONS",
            "-c timezone=UTC -c intervalstyle=iso_8601 -c bytea_output=hex \
             -c datestyle=ISO,MDY -c extra_float_digits=3",
        )
        .args(["-v", "ON_ERROR_STOP=1", "-tAq", url, "-c", sql])
        .output()
        .map_err(|e| format!("running psql: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).to_string())
    }
}

/// Path to the seedle binary under test.
fn seedle_bin() -> PathBuf {
    // The test executable lives in target/<profile>/deps/, so the binary is two
    // levels up.
    let mut p = std::env::current_exe().expect("test exe path");
    p.pop();
    if p.ends_with("deps") {
        p.pop();
    }
    p.join("seedle")
}

/// Recursively compare two directories, returning a description of the first
/// difference found, or `None` when they match byte for byte.
pub fn dirs_differ(a: &Path, b: &Path) -> Option<String> {
    let list = |root: &Path| -> Vec<(String, Vec<u8>)> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    let rel = p
                        .strip_prefix(root)
                        .expect("under root")
                        .to_string_lossy()
                        .to_string();
                    out.push((rel, std::fs::read(&p).unwrap_or_default()));
                }
            }
        }
        out.sort_by(|x, y| x.0.cmp(&y.0));
        out
    };

    let (ax, bx) = (list(a), list(b));
    let names_a: Vec<&String> = ax.iter().map(|(n, _)| n).collect();
    let names_b: Vec<&String> = bx.iter().map(|(n, _)| n).collect();
    if names_a != names_b {
        return Some(format!("file lists differ:\n  {names_a:?}\n  {names_b:?}"));
    }
    for ((name, abytes), (_, bbytes)) in ax.iter().zip(&bx) {
        if abytes != bbytes {
            return Some(format!(
                "{name} differs:\n--- {}\n{}\n--- {}\n{}",
                a.display(),
                String::from_utf8_lossy(abytes),
                b.display(),
                String::from_utf8_lossy(bbytes)
            ));
        }
    }
    None
}

/// The schema most tests use: foreign keys, an enum, a generated column, a
/// self-reference, and a table of awkward types.
pub const SCHEMA: &str = r#"
CREATE TYPE tier AS ENUM ('free', 'pro', 'team');

CREATE TABLE orgs (
    id      bigserial PRIMARY KEY,
    name    varchar(80) NOT NULL UNIQUE,
    plan    tier NOT NULL DEFAULT 'free'
);

CREATE TABLE users (
    id          uuid PRIMARY KEY,
    org_id      bigint NOT NULL REFERENCES orgs(id),
    email       varchar(120) NOT NULL UNIQUE,
    display     text,
    prefs       jsonb,
    created_at  timestamptz NOT NULL DEFAULT now(),
    email_lower text GENERATED ALWAYS AS (lower(email)) STORED
);

CREATE TABLE orders (
    id        bigserial PRIMARY KEY,
    user_id   uuid NOT NULL REFERENCES users(id),
    amount    numeric(14,4) NOT NULL,
    placed_on date NOT NULL
);

CREATE TABLE employees (
    id         integer PRIMARY KEY,
    manager_id integer REFERENCES employees(id),
    name       text NOT NULL
);

CREATE TABLE countries (
    code char(2) PRIMARY KEY,
    name text NOT NULL
);

CREATE TABLE templates (
    slug    text PRIMARY KEY,
    subject text NOT NULL,
    body    jsonb NOT NULL
);

CREATE TABLE torture (
    id         integer PRIMARY KEY,
    t_bool     boolean,
    t_i16      smallint,
    t_i64      bigint,
    t_f32      real,
    t_f64      double precision,
    t_num      numeric(38,10),
    t_text     text,
    t_vc       varchar(10),
    t_bytes    bytea,
    t_uuid     uuid,
    t_json     json,
    t_jsonb    jsonb,
    t_date     date,
    t_time     time,
    t_ts       timestamp,
    t_tstz     timestamptz,
    t_interval interval,
    t_enum     tier,
    t_int_arr  integer[],
    t_text_arr text[]
);
"#;

/// Rows covering nulls, unicode, embedded quotes and newlines, extreme numbers,
/// the DST-repeat hour, and every exotic type.
pub const DATA: &str = r#"
INSERT INTO countries VALUES ('FR','France'), ('US','United States'), ('JP','日本');
INSERT INTO orgs (name, plan) VALUES ('Acme','pro'), ('Globex','free'), ('Initech','team');

INSERT INTO users (id, org_id, email, display, prefs, created_at) VALUES
  ('9f2c4b1e-7a3d-4e5f-8b9c-0d1e2f3a4b5c', 1, 'admin@acme.test', 'Admin', '{"theme":"dark","n":1}', '2024-01-01 12:00:00+00'),
  ('0d1e2f3a-4b5c-6d7e-8f90-1a2b3c4d5e6f', 1, 'ops@acme.test', NULL, NULL, '2024-03-15 08:30:00+02'),
  ('11111111-2222-3333-4444-555555555555', 2, 'hi@globex.test', E'Multi\nline "quoted" 🌱', '{"z":1,"a":2}', '2024-07-01 00:00:00+00'),
  ('66666666-7777-8888-9999-aaaaaaaaaaaa', 3, 'admin@initech.test', '  padded  ', '[]', '2024-11-03 01:30:00-04');

INSERT INTO orders (user_id, amount, placed_on) VALUES
  ('9f2c4b1e-7a3d-4e5f-8b9c-0d1e2f3a4b5c', 19.9900, '2024-02-29'),
  ('9f2c4b1e-7a3d-4e5f-8b9c-0d1e2f3a4b5c', 0.0001, '2024-03-01'),
  ('11111111-2222-3333-4444-555555555555', 1234567890.1234, '2024-07-04');

INSERT INTO employees VALUES (1, NULL, 'Boss'), (2, 1, 'Manager'), (3, 2, 'Worker');

INSERT INTO templates VALUES
  ('welcome-email', 'Welcome!', '{"blocks":[{"type":"text","value":"Hi there"}],"version":2}'),
  ('password-reset', 'Reset your password', '{"blocks":[],"version":1}');

INSERT INTO torture VALUES (
  1, true, -32768, 9223372036854775807, 1.5, 3.141592653589793,
  12345678901234567890.1234567890,
  E'tabs\there, newlines\nhere, quote", backslash\\, emoji 🌱',
  '', '\xdeadbeef00ff'::bytea, '9f2c4b1e-7a3d-4e5f-8b9c-0d1e2f3a4b5c',
  '{"b":1,"a":2}', '{"b":1,"a":2}',
  '2024-02-29', '13:45:30.5', '2024-01-01 00:00:00', '2024-11-03 01:30:00-04',
  '1 hour 30 minutes', 'pro', '{1,2,3}', '{"a b","c,d",""}'
);
INSERT INTO torture (id, t_f64, t_num) VALUES
  (2, 'NaN', 'NaN'), (3, 'Infinity', 0.0000000000), (4, '-Infinity', -0.0000000001);
INSERT INTO torture (id) VALUES (5);
"#;

/// The table list most tests use.
pub const TABLES: &str = "  countries: {}\n  \
                          orgs: {}\n  \
                          users: {}\n  \
                          orders: {}\n  \
                          employees: {}\n  \
                          torture: {}\n  \
                          templates: {}\n";

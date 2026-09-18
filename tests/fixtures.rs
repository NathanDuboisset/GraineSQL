//! Committed seed files, replayed against every engine that can hold them.
//!
//! Each fixture in `tests/fixtures/` is a whole project: a config, a schema per
//! engine, the data it was built from, and the seed files. Two checks run per
//! fixture:
//!
//! - **anchor**: build a database from `data.sql`, export, and compare to the
//!   committed files, tying them to a source GraineSQL never wrote;
//! - **replay**: for each declared engine, load the committed files and export
//!   them again, which must be byte-identical.
//!
//! Replay alone would pass on a file someone had edited, since the loader and
//! the exporter would simply agree with each other. The anchor is what makes an
//! edited or stale fixture fail.
//!
//! Regenerate with `UPDATE_FIXTURES=1 cargo test --test fixtures`. The resulting
//! diff is the point, so do it deliberately.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::graine_bin;

/// Fixture name and the engines whose schema it ships.
const FIXTURES: &[(&str, &[Engine])] = &[
    (
        "ecommerce",
        &[Engine::Postgres, Engine::Mysql, Engine::Sqlite],
    ),
    ("cms", &[Engine::Postgres, Engine::Mysql, Engine::Sqlite]),
    ("graph", &[Engine::Postgres, Engine::Mysql, Engine::Sqlite]),
    // SQLite has no exact decimal, so it cannot hold these rows unchanged.
    ("analytics", &[Engine::Postgres, Engine::Mysql]),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Engine {
    Postgres,
    Mysql,
    Sqlite,
}

impl Engine {
    fn name(self) -> &'static str {
        match self {
            Engine::Postgres => "postgres",
            Engine::Mysql => "mysql",
            Engine::Sqlite => "sqlite",
        }
    }

    /// Base URL for this engine, or `None` when it is not configured.
    fn base_url(self) -> Option<String> {
        let var = match self {
            Engine::Postgres => "GRAINE_TEST_PG",
            Engine::Mysql => "GRAINE_TEST_MYSQL",
            // SQLite is a file, so it needs nothing configured.
            Engine::Sqlite => return Some(String::new()),
        };
        std::env::var(var).ok().filter(|u| !u.trim().is_empty())
    }
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// A fixture copied into a scratch directory, wired to one engine.
struct Run {
    dir: tempfile::TempDir,
    engine: Engine,
    fixture: String,
    /// Set for the server engines so `Drop` can clean up.
    database: Option<(String, String)>,
    rt: tokio::runtime::Runtime,
}

impl Run {
    fn new(fixture: &str, engine: Engine, base: &str) -> Run {
        let dir = tempfile::tempdir().expect("scratch dir");
        let src = fixtures_dir().join(fixture);

        // Copy the config and the committed seed files; the schema and data
        // stay where they are.
        std::fs::copy(src.join("graine.yaml"), dir.path().join("graine.yaml")).unwrap();
        let seed = src.join("seed");
        if seed.is_dir() {
            copy_dir(&seed, &dir.path().join("seed"));
        }

        let mut run = Run {
            dir,
            engine,
            fixture: fixture.to_string(),
            database: None,
            rt: tokio::runtime::Runtime::new().expect("tokio runtime"),
        };
        run.create_database(base);
        run
    }

    fn schema_sql(&self) -> String {
        let path = fixtures_dir()
            .join(&self.fixture)
            .join(format!("schema.{}.sql", self.engine.name()));
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    fn data_sql(&self) -> String {
        std::fs::read_to_string(fixtures_dir().join(&self.fixture).join("data.sql"))
            .expect("data.sql")
    }

    /// Create a scratch database and apply the fixture's schema.
    fn create_database(&mut self, base: &str) {
        let name = format!("graine_fx_{}_{}", self.fixture, self.engine.name());
        match self.engine {
            Engine::Sqlite => {
                let path = self.dir.path().join("db.sqlite");
                let url = format!("sqlite://{}?mode=rwc", path.display());
                self.database = Some((url.clone(), String::new()));
                self.exec(&url, &self.schema_sql())
                    .expect("applying the schema");
            }
            _ => {
                let base = base.trim_end_matches('/');
                let admin = match self.engine {
                    Engine::Postgres => format!("{base}/postgres"),
                    _ => format!("{base}/mysql"),
                };
                let _ = self.exec(&admin, &format!("DROP DATABASE IF EXISTS {name}"));
                self.exec(&admin, &format!("CREATE DATABASE {name}"))
                    .expect("creating the database");
                let url = format!("{base}/{name}");
                self.database = Some((url.clone(), admin));
                self.exec(&url, &self.schema_sql())
                    .expect("applying the schema");
            }
        }
    }

    fn url(&self) -> String {
        self.database.as_ref().expect("database").0.clone()
    }

    /// Run statements one at a time, since the drivers take one per call.
    fn exec(&self, url: &str, sql: &str) -> Result<(), String> {
        self.rt.block_on(async {
            use sqlx::Executor;
            for stmt in split_statements(sql) {
                match self.engine {
                    Engine::Postgres => {
                        let pool = sqlx::PgPool::connect(url)
                            .await
                            .map_err(|e| format!("connecting: {e}"))?;
                        pool.execute(stmt.as_str())
                            .await
                            .map_err(|e| format!("{stmt}: {e}"))?;
                    }
                    Engine::Mysql => {
                        let pool = sqlx::MySqlPool::connect(url)
                            .await
                            .map_err(|e| format!("connecting: {e}"))?;
                        pool.execute(stmt.as_str())
                            .await
                            .map_err(|e| format!("{stmt}: {e}"))?;
                    }
                    Engine::Sqlite => {
                        let pool = sqlx::SqlitePool::connect(url)
                            .await
                            .map_err(|e| format!("connecting: {e}"))?;
                        pool.execute(stmt.as_str())
                            .await
                            .map_err(|e| format!("{stmt}: {e}"))?;
                    }
                }
            }
            Ok(())
        })
    }

    /// Run graine with this engine selected and its URL in the environment.
    fn run(&self, args: &[&str]) -> std::process::Output {
        Command::new(graine_bin())
            .args(args)
            .args(["--source", self.engine.name()])
            .current_dir(self.dir.path())
            .env("GRAINE_FIXTURE_PG", url_for(self, Engine::Postgres))
            .env("GRAINE_FIXTURE_MYSQL", url_for(self, Engine::Mysql))
            .env("GRAINE_FIXTURE_SQLITE", url_for(self, Engine::Sqlite))
            .output()
            .expect("running graine")
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "{} on {}: {args:?} failed\n{}\n{}",
            self.fixture,
            self.engine.name(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        // Best effort: a leftover database only affects the next run, which
        // drops it first. Panicking here would abort the test process.
        if let Some((_, admin)) = &self.database
            && !admin.is_empty()
        {
            let name = format!("graine_fx_{}_{}", self.fixture, self.engine.name());
            let _ = self.exec(admin, &format!("DROP DATABASE IF EXISTS {name}"));
        }
    }
}

/// Only the engine under test has a real URL; the others just have to parse.
fn url_for(run: &Run, engine: Engine) -> String {
    if engine == run.engine {
        run.url()
    } else {
        match engine {
            Engine::Postgres => "postgres://unused@127.0.0.1/unused".into(),
            Engine::Mysql => "mysql://unused@127.0.0.1/unused".into(),
            Engine::Sqlite => "sqlite://unused.db".into(),
        }
    }
}

/// Split a script on semicolons that are not inside a quoted literal.
fn split_statements(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = sql.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '-' if !in_quotes && chars.peek() == Some(&'-') => {
                for skipped in chars.by_ref() {
                    if skipped == '\n' {
                        break;
                    }
                }
                current.push(' ');
            }
            '\'' => {
                if in_quotes && chars.peek() == Some(&'\'') {
                    current.push_str("''");
                    chars.next();
                } else {
                    in_quotes = !in_quotes;
                    current.push('\'');
                }
            }
            ';' if !in_quotes => {
                if !current.trim().is_empty() {
                    out.push(current.trim().to_string());
                }
                current.clear();
            }
            c => current.push(c),
        }
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_string());
    }
    out
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// Compare two directories, ignoring the lock, which is engine-specific.
fn seed_files_differ(a: &Path, b: &Path) -> Option<String> {
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
                } else if p.file_name().and_then(|n| n.to_str()) != Some("graine.lock") {
                    let rel = p.strip_prefix(root).unwrap().to_string_lossy().to_string();
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
    for ((name, a_bytes), (_, b_bytes)) in ax.iter().zip(&bx) {
        if a_bytes != b_bytes {
            return Some(format!(
                "{name} differs:\n--- committed\n{}\n--- exported\n{}",
                String::from_utf8_lossy(a_bytes),
                String::from_utf8_lossy(b_bytes)
            ));
        }
    }
    None
}

/// Rebuild a fixture's seed files from its `data.sql`.
fn regenerate(fixture: &str, engine: Engine, base: &str) {
    let run = Run::new(fixture, engine, base);
    run.exec(&run.url(), &run.data_sql())
        .unwrap_or_else(|e| panic!("{fixture}: loading data.sql: {e}"));
    run.ok(&["lock", "-q"]);
    run.ok(&["export", "-q"]);

    let target = fixtures_dir().join(fixture).join("seed");
    let _ = std::fs::remove_dir_all(&target);
    copy_dir(&run.dir.path().join("seed"), &target);
    // The lock is engine-specific and regenerated per run, so it is not part of
    // the fixture.
    let _ = std::fs::remove_file(target.join("graine.lock"));
    eprintln!("regenerated {fixture} from {}", engine.name());
}

#[test]
fn every_fixture_round_trips_on_every_engine_it_declares() {
    let regen = std::env::var("UPDATE_FIXTURES").is_ok();
    let mut ran = 0;

    for (fixture, engines) in FIXTURES {
        // Fixtures are generated from Postgres, so that is the reference.
        if regen {
            if let Some(base) = Engine::Postgres.base_url() {
                regenerate(fixture, Engine::Postgres, &base);
            } else {
                eprintln!("skipping regeneration: GRAINE_TEST_PG is not set");
            }
        }

        if !fixtures_dir().join(fixture).join("seed").is_dir() {
            eprintln!("skipping {fixture}: no seed files; run with UPDATE_FIXTURES=1");
            continue;
        }

        // Anchor the fixture: building from `data.sql` ties the committed
        // files to a source GraineSQL never wrote.
        if let Some(base) = Engine::Postgres.base_url() {
            let run = Run::new(fixture, Engine::Postgres, &base);
            run.exec(&run.url(), &run.data_sql())
                .unwrap_or_else(|e| panic!("{fixture}: loading data.sql: {e}"));
            run.ok(&["lock", "-q"]);
            run.ok(&["export", "-o", "out", "-q"]);

            if let Some(d) = seed_files_differ(
                &fixtures_dir().join(fixture).join("seed"),
                &run.dir.path().join("out"),
            ) {
                panic!(
                    "{fixture}: the committed seed files do not match data.sql.\n\
                     Either a value stopped exporting the way it used to, or the \
                     fixture needs regenerating with UPDATE_FIXTURES=1.\n{d}"
                );
            }
            eprintln!("ok: {fixture} matches data.sql");
            ran += 1;
        }

        for engine in *engines {
            let Some(base) = engine.base_url() else {
                eprintln!("skipping {fixture} on {}: not configured", engine.name());
                continue;
            };

            let run = Run::new(fixture, *engine, &base);
            run.ok(&["lock", "-q"]);
            run.ok(&["load", "--yes", "-q"]);
            run.ok(&["export", "-o", "out", "-q"]);

            if let Some(d) = seed_files_differ(
                &fixtures_dir().join(fixture).join("seed"),
                &run.dir.path().join("out"),
            ) {
                panic!("{fixture} does not reproduce on {}:\n{d}", engine.name());
            }
            eprintln!("ok: {fixture} on {}", engine.name());
            ran += 1;
        }
    }

    assert!(
        ran > 0 || std::env::var("GRAINE_TEST_PG").is_err(),
        "no fixture ran despite a configured database"
    );
}

/// Engine pairs whose lock travels with the seed files.
///
/// The replay above always re-locks against the target first, which throws away
/// the source's lock and so never exercises the interesting case: a lock written
/// by one engine checked against another.
const CROSS: &[(&str, Engine, &[Engine])] = &[
    ("cms", Engine::Postgres, &[Engine::Mysql, Engine::Sqlite]),
    ("graph", Engine::Postgres, &[Engine::Mysql, Engine::Sqlite]),
    (
        "ecommerce",
        Engine::Postgres,
        &[Engine::Mysql, Engine::Sqlite],
    ),
    // SQLite has no exact decimal, so analytics cannot land there.
    ("analytics", Engine::Postgres, &[Engine::Mysql]),
    // The directions nothing covered: these narrow rather than widen, which is
    // where the type classes are most likely to refuse.
    ("cms", Engine::Sqlite, &[Engine::Postgres, Engine::Mysql]),
    ("graph", Engine::Mysql, &[Engine::Postgres, Engine::Sqlite]),
];

#[test]
fn a_lock_and_its_seed_files_load_into_another_engine() {
    let mut ran = 0;

    for (fixture, from, targets) in CROSS {
        let Some(from_base) = from.base_url() else {
            continue;
        };

        // Export from the source engine, keeping its lock.
        let src = Run::new(fixture, *from, &from_base);
        src.exec(&src.url(), &src.data_sql())
            .unwrap_or_else(|e| panic!("{fixture}: loading data.sql: {e}"));
        src.ok(&["lock", "-q"]);
        src.ok(&["export", "-o", "out", "-q"]);

        for to in *targets {
            let Some(to_base) = to.base_url() else {
                continue;
            };
            let dst = Run::new(fixture, *to, &to_base);

            // The source's lock travels with the files; the target never locks.
            let seed = dst.dir.path().join("seed");
            let _ = std::fs::remove_dir_all(&seed);
            copy_dir(&src.dir.path().join("out"), &seed);

            dst.ok(&["load", "--yes", "-q"]);

            // Re-exporting from the target reproduces the same rows, which is a
            // stronger claim than a row count.
            dst.ok(&["lock", "-q"]);
            dst.ok(&["export", "-o", "back", "-q"]);
            if let Some(d) = seed_files_differ(&seed, &dst.dir.path().join("back")) {
                panic!(
                    "{fixture}: {} -> {} did not round-trip:\n{d}",
                    from.name(),
                    to.name()
                );
            }
            eprintln!("ok: {fixture} {} -> {}", from.name(), to.name());
            ran += 1;
        }
    }

    assert!(
        ran > 0 || std::env::var("GRAINE_TEST_PG").is_err(),
        "no cross-engine pair ran despite a configured database"
    );
}

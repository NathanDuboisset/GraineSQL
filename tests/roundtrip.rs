//! The properties that make seed files worth committing: exports are
//! reproducible, and export/load are exact inverses.

mod common;

use common::{DATA, Fixture, SCHEMA, TABLES, dirs_differ};

/// Export twice into separate directories and compare.
fn export_twice(f: &Fixture, source: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let a = f.path().join("out-a");
    let b = f.path().join("out-b");
    f.ok(&["export", "--source", source, "-o", "out-a", "-q"]);
    f.ok(&["export", "--source", source, "-o", "out-b", "-q"]);
    (a, b)
}

#[test]
fn exporting_the_same_data_twice_is_byte_identical() {
    let base = require_pg!();
    let f = Fixture::new("determinism", &base, SCHEMA);
    f.sql_src(DATA).unwrap();
    f.write_config(TABLES);
    f.ok(&["lock", "-q"]);

    let (a, b) = export_twice(&f, "src");
    if let Some(d) = dirs_differ(&a, &b) {
        panic!(
            "two exports of unchanged data differ, so every export would produce diff noise:\n{d}"
        );
    }
}

#[test]
fn concurrency_cannot_change_the_output() {
    let base = require_pg!();
    let f = Fixture::new("concurrency", &base, SCHEMA);
    f.sql_src(DATA).unwrap();

    f.write_config_with("export:\n  concurrency: 1\n", TABLES);
    f.ok(&["lock", "-q"]);
    f.ok(&["export", "-o", "out-serial", "-q"]);

    f.write_config_with("export:\n  concurrency: 8\n", TABLES);
    f.ok(&["export", "-o", "out-parallel", "-q"]);

    if let Some(d) = dirs_differ(&f.path().join("out-serial"), &f.path().join("out-parallel")) {
        panic!("export output depends on how many tables run at once:\n{d}");
    }
}

/// The single most valuable test in the suite: it exercises every encoder and
/// decoder at once and fails on any asymmetry between them.
#[test]
fn export_then_load_then_export_is_byte_identical() {
    let base = require_pg!();
    let f = Fixture::new("roundtrip", &base, SCHEMA);
    f.sql_src(DATA).unwrap();
    f.write_config(TABLES);
    f.ok(&["lock", "-q"]);

    f.ok(&["export", "-o", "out-src", "-q"]);
    // The lock the load reads lives in the configured directory.
    f.ok(&["export", "-q"]);
    f.ok(&["load", "--source", "dst", "--yes", "-q"]);
    f.ok(&["export", "--source", "dst", "-o", "out-dst", "-q"]);

    if let Some(d) = dirs_differ(&f.path().join("out-src"), &f.path().join("out-dst")) {
        panic!("a value changed somewhere between export and load:\n{d}");
    }
}

#[test]
fn round_trip_holds_for_every_format() {
    let base = require_pg!();
    for (i, format) in ["jsonl", "jsonl.gz", "csv", "sql"].iter().enumerate() {
        let f = Fixture::new(&format!("rt_fmt{i}"), &base, SCHEMA);
        f.sql_src(DATA).unwrap();
        f.write_config_with(&format!("export:\n  format: {format}\n"), TABLES);
        f.ok(&["lock", "-q"]);

        f.ok(&["export", "-o", "out-src", "-q"]);
        f.ok(&["export", "-q"]);
        f.ok(&["load", "--source", "dst", "--yes", "-q"]);
        f.ok(&["export", "--source", "dst", "-o", "out-dst", "-q"]);

        if let Some(d) = dirs_differ(&f.path().join("out-src"), &f.path().join("out-dst")) {
            panic!("{format} does not round-trip:\n{d}");
        }
    }
}

#[test]
fn compressed_output_is_smaller_and_still_reproducible() {
    let base = require_pg!();
    let f = Fixture::new("gz", &base, SCHEMA);
    f.sql_src(DATA).unwrap();

    f.write_config_with("export:\n  format: jsonl\n", TABLES);
    f.ok(&["lock", "-q"]);
    f.ok(&["export", "-o", "plain", "-q"]);

    f.write_config_with("export:\n  format: jsonl.gz\n", TABLES);
    f.ok(&["export", "-o", "gz-a", "-q"]);
    f.ok(&["export", "-o", "gz-b", "-q"]);

    // gzip stores an mtime in its header, so this only holds because the
    // writer zeroes it.
    if let Some(d) = dirs_differ(&f.path().join("gz-a"), &f.path().join("gz-b")) {
        panic!("compressed output is not reproducible:\n{d}");
    }

    let size = |dir: &str, name: &str| {
        std::fs::metadata(f.path().join(dir).join(name))
            .unwrap_or_else(|e| panic!("{dir}/{name}: {e}"))
            .len()
    };
    assert!(
        size("gz-a", "torture.jsonl.gz") < size("plain", "torture.jsonl"),
        "compression should shrink the file"
    );
}

#[test]
fn bulk_and_per_row_loading_agree() {
    // insert and truncate_first go through COPY on Postgres while upsert stays
    // per-row; both paths must produce the same rows.
    let base = require_pg!();
    let f = Fixture::new("bulk", &base, SCHEMA);
    f.sql_src(DATA).unwrap();

    let bulk = TABLES.replace(": {}", ":\n    load: truncate_first");
    f.write_config(&bulk);
    f.ok(&["lock", "-q"]);
    f.ok(&["export", "-o", "out-src", "-q"]);
    f.ok(&["export", "-q"]);
    f.ok(&["load", "--source", "dst", "--yes", "-q"]);
    f.ok(&["export", "--source", "dst", "-o", "out-bulk", "-q"]);

    if let Some(d) = dirs_differ(&f.path().join("out-src"), &f.path().join("out-bulk")) {
        panic!("a bulk load changed the data:\n{d}");
    }

    // The awkward values are the ones a CSV-framed COPY stream could mangle.
    assert_eq!(
        f.query_dst("SELECT t_num::text FROM torture WHERE id = 1"),
        "12345678901234567890.1234567890"
    );
    assert_eq!(
        f.query_dst("SELECT encode(t_bytes, 'hex') FROM torture WHERE id = 1"),
        "deadbeef00ff"
    );
    assert_eq!(
        f.query_dst("SELECT length(t_vc) FROM torture WHERE id = 1"),
        "0"
    );
    assert_eq!(
        f.query_dst("SELECT (t_text IS NULL)::text FROM torture WHERE id = 2"),
        "true"
    );
    assert_eq!(
        f.query_dst("SELECT display FROM users WHERE email = 'hi@globex.test'"),
        "Multi\nline \"quoted\" 🌱"
    );
}

#[test]
fn per_row_layout_round_trips() {
    let base = require_pg!();
    let f = Fixture::new("perrow", &base, SCHEMA);
    f.sql_src(DATA).unwrap();
    f.write_config("  templates:\n    layout: per_row\n    format: json\n    pretty: true\n");
    f.ok(&["lock", "-q"]);
    f.ok(&["export", "-q"]);

    // One file per row, named from the primary key, and pretty-printed.
    let welcome = f.read_seed("templates/welcome-email.json");
    assert!(
        welcome.starts_with("{\n  \"slug\""),
        "not pretty-printed:\n{welcome}"
    );
    assert!(f.seed_dir().join("templates/password-reset.json").exists());

    f.ok(&["export", "-o", "out-src", "-q"]);
    f.ok(&["load", "--source", "dst", "--yes", "-q"]);
    f.ok(&["export", "--source", "dst", "-o", "out-dst", "-q"]);
    if let Some(d) = dirs_differ(&f.path().join("out-src"), &f.path().join("out-dst")) {
        panic!("per-row layout does not round-trip:\n{d}");
    }
}

#[test]
fn loading_twice_converges_rather_than_duplicating() {
    let base = require_pg!();
    let f = Fixture::new("idempotent", &base, SCHEMA);
    f.sql_src(DATA).unwrap();
    f.write_config(TABLES);
    f.ok(&["lock", "-q"]);
    f.ok(&["export", "-q"]);

    f.ok(&["load", "--source", "dst", "--yes", "-q"]);
    let after_first = f.query_dst("SELECT count(*) FROM users");
    f.ok(&["load", "--source", "dst", "--yes", "-q"]);
    let after_second = f.query_dst("SELECT count(*) FROM users");

    assert_eq!(after_first, "4");
    assert_eq!(
        after_first, after_second,
        "the second upsert duplicated rows"
    );

    f.ok(&["export", "-o", "out-src", "-q"]);
    f.ok(&["export", "--source", "dst", "-o", "out-dst", "-q"]);
    assert!(
        dirs_differ(&f.path().join("out-src"), &f.path().join("out-dst")).is_none(),
        "a second load changed the data"
    );
}

#[test]
fn every_exported_line_is_valid_json() {
    let base = require_pg!();
    let f = Fixture::new("validjson", &base, SCHEMA);
    f.sql_src(DATA).unwrap();
    f.write_config(TABLES);
    f.ok(&["lock", "-q"]);
    f.ok(&["export", "-q"]);

    // The torture table is the one that catches this: `numeric` NaN and
    // non-finite floats have no JSON literal form.
    let mut lines = 0;
    for entry in std::fs::read_dir(f.seed_dir()).unwrap().flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        for (i, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            serde_json::from_str::<serde_json::Value>(line).unwrap_or_else(|e| {
                panic!(
                    "{}:{} is not valid json: {e}\n{line}",
                    path.display(),
                    i + 1
                )
            });
            lines += 1;
        }
    }
    assert!(lines > 0, "no lines were checked");
}

#[test]
fn awkward_values_survive_a_load_intact() {
    let base = require_pg!();
    let f = Fixture::new("values", &base, SCHEMA);
    f.sql_src(DATA).unwrap();
    f.write_config(TABLES);
    f.ok(&["lock", "-q"]);
    f.ok(&["export", "-q"]);
    f.ok(&["load", "--source", "dst", "--yes", "-q"]);

    // Each of these is a value a naive exporter gets wrong.
    let checks: &[(&str, &str)] = &[
        // Exact decimal digits, not a float approximation.
        (
            "SELECT t_num::text FROM torture WHERE id = 1",
            "12345678901234567890.1234567890",
        ),
        // Trailing zeros are preserved by the numeric type itself.
        ("SELECT amount::text FROM orders WHERE amount < 1", "0.0001"),
        // Non-finite floats.
        ("SELECT t_f64::text FROM torture WHERE id = 2", "NaN"),
        ("SELECT t_f64::text FROM torture WHERE id = 3", "Infinity"),
        ("SELECT t_f64::text FROM torture WHERE id = 4", "-Infinity"),
        ("SELECT t_num::text FROM torture WHERE id = 2", "NaN"),
        // Bytes, including a NUL and a high byte.
        (
            "SELECT encode(t_bytes, 'hex') FROM torture WHERE id = 1",
            "deadbeef00ff",
        ),
        // Empty string is not NULL.
        ("SELECT length(t_vc) FROM torture WHERE id = 1", "0"),
        (
            "SELECT (t_vc IS NULL)::text FROM torture WHERE id = 1",
            "false",
        ),
        // NULL is not an empty string.
        (
            "SELECT (t_text IS NULL)::text FROM torture WHERE id = 2",
            "true",
        ),
        // Arrays and enums.
        (
            "SELECT t_int_arr::text FROM torture WHERE id = 1",
            "{1,2,3}",
        ),
        ("SELECT t_enum::text FROM torture WHERE id = 1", "pro"),
        // Interval.
        (
            "SELECT t_interval::text FROM torture WHERE id = 1",
            "PT1H30M",
        ),
        // The DST-repeat instant keeps its offset meaning.
        (
            "SELECT t_tstz::text FROM torture WHERE id = 1",
            "2024-11-03 05:30:00+00",
        ),
        // Sub-second precision.
        (
            "SELECT t_time::text FROM torture WHERE id = 1",
            "13:45:30.5",
        ),
        // Unicode and embedded control characters.
        (
            "SELECT display FROM users WHERE email = 'hi@globex.test'",
            "Multi\nline \"quoted\" 🌱",
        ),
        // Leading and trailing spaces.
        (
            "SELECT '[' || display || ']' FROM users WHERE email = 'admin@initech.test'",
            "[  padded  ]",
        ),
        ("SELECT name FROM countries WHERE code = 'JP'", "日本"),
    ];

    for (sql, expected) in checks {
        assert_eq!(
            f.query_dst(sql),
            *expected,
            "value did not survive the round trip: {sql}"
        );
    }
}

#[test]
fn generated_columns_are_never_written_but_are_recomputed() {
    let base = require_pg!();
    let f = Fixture::new("generated", &base, SCHEMA);
    f.sql_src(DATA).unwrap();
    f.write_config(TABLES);
    f.ok(&["lock", "-q"]);
    f.ok(&["export", "-q"]);

    let users = f.read_seed("users.jsonl");
    assert!(
        !users.contains("email_lower"),
        "a generated column cannot be inserted, so exporting it makes an unloadable file:\n{users}"
    );

    // It still ends up correct in the target, computed by the database.
    f.ok(&["load", "--source", "dst", "--yes", "-q"]);
    assert_eq!(
        f.query_dst("SELECT email_lower FROM users WHERE email = 'admin@acme.test'"),
        "admin@acme.test"
    );
}

#[test]
fn identity_sequences_are_advanced_past_the_loaded_rows() {
    let base = require_pg!();
    let f = Fixture::new("sequences", &base, SCHEMA);
    f.sql_src(DATA).unwrap();
    f.write_config(TABLES);
    f.ok(&["lock", "-q"]);
    f.ok(&["export", "-q"]);
    f.ok(&["load", "--source", "dst", "--yes", "-q"]);

    // Without the sequence fixup this insert reuses id 1 and violates the key.
    let new_id = f.query_dst("INSERT INTO orgs (name) VALUES ('Fresh') RETURNING id");
    assert_eq!(
        new_id, "4",
        "the next insert must not collide with a seeded id"
    );
}

//! Storage bucket export and load.
//!
//! These need a Supabase-compatible storage service, which is a separate
//! dependency from the database, so they are gated on their own variables:
//! `SEEDLE_TEST_STORAGE_URL` and `SEEDLE_TEST_STORAGE_KEY` (a service-role key).

mod common;

use std::process::Command;

use common::{Fixture, dirs_differ};

/// Skip unless a storage service is configured.
macro_rules! require_storage {
    () => {{
        let url = std::env::var("SEEDLE_TEST_STORAGE_URL").unwrap_or_default();
        let key = std::env::var("SEEDLE_TEST_STORAGE_KEY").unwrap_or_default();
        if url.trim().is_empty() || key.trim().is_empty() {
            eprintln!(
                "skipping: set SEEDLE_TEST_STORAGE_URL and SEEDLE_TEST_STORAGE_KEY to run the \
                 bucket tests"
            );
            return;
        }
        (url, key)
    }};
}

const SCHEMA: &str = "CREATE TABLE notes (id int PRIMARY KEY, body text);";

/// Minimal curl-based storage helpers: the tests must not depend on the code
/// under test to set up their own fixtures.
struct Storage {
    url: String,
    key: String,
    bucket: String,
}

impl Storage {
    fn new(url: &str, key: &str, bucket: &str) -> Storage {
        let s = Storage {
            url: url.trim_end_matches('/').to_string(),
            key: key.to_string(),
            bucket: bucket.to_string(),
        };
        s.delete_bucket();
        s.create_bucket();
        s
    }

    fn curl(&self, args: &[&str]) -> String {
        let out = Command::new("curl")
            .args(["-s", "-H", &format!("Authorization: Bearer {}", self.key)])
            .args(args)
            .output()
            .expect("running curl");
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    fn create_bucket(&self) {
        self.curl(&[
            "-X",
            "POST",
            &format!("{}/storage/v1/bucket", self.url),
            "-H",
            "Content-Type: application/json",
            "-d",
            &format!(
                "{{\"id\":\"{}\",\"name\":\"{}\",\"public\":false}}",
                self.bucket, self.bucket
            ),
        ]);
    }

    fn delete_bucket(&self) {
        // Emptying is required before a bucket can be dropped.
        self.curl(&[
            "-X",
            "POST",
            &format!("{}/storage/v1/bucket/{}/empty", self.url, self.bucket),
        ]);
        self.curl(&[
            "-X",
            "DELETE",
            &format!("{}/storage/v1/bucket/{}", self.url, self.bucket),
        ]);
    }

    fn put(&self, key: &str, bytes: &[u8]) {
        let tmp = std::env::temp_dir().join(format!("seedle-upload-{}", sanitize(key)));
        std::fs::write(&tmp, bytes).unwrap();
        self.curl(&[
            "-X",
            "POST",
            &format!(
                "{}/storage/v1/object/{}/{}",
                self.url,
                self.bucket,
                enc(key)
            ),
            "-H",
            "x-upsert: true",
            "--data-binary",
            &format!("@{}", tmp.display()),
        ]);
        let _ = std::fs::remove_file(tmp);
    }

    fn get(&self, key: &str) -> Vec<u8> {
        let out = Command::new("curl")
            .args([
                "-s",
                "-H",
                &format!("Authorization: Bearer {}", self.key),
                &format!(
                    "{}/storage/v1/object/{}/{}",
                    self.url,
                    self.bucket,
                    enc(key)
                ),
            ])
            .output()
            .expect("running curl");
        out.stdout
    }

    /// Flip the bucket's visibility, to produce a settings change.
    fn set_public(&self, public: bool) {
        self.curl(&[
            "-X",
            "PUT",
            &format!("{}/storage/v1/bucket/{}", self.url, self.bucket),
            "-H",
            "Content-Type: application/json",
            "-d",
            &format!("{{\"id\":\"{}\",\"public\":{public}}}", self.bucket),
        ]);
    }

    fn delete(&self, key: &str) {
        self.curl(&[
            "-X",
            "DELETE",
            &format!(
                "{}/storage/v1/object/{}/{}",
                self.url,
                self.bucket,
                enc(key)
            ),
        ]);
    }
}

impl Drop for Storage {
    fn drop(&mut self) {
        self.delete_bucket();
    }
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Percent-encode an object key for a URL, keeping `/` as a separator.
///
/// curl will not do this, and a raw space or accent in the path makes a
/// malformed request that silently uploads nothing.
fn enc(key: &str) -> String {
    key.split('/')
        .map(|seg| {
            seg.bytes()
                .map(|b| match b {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                        (b as char).to_string()
                    }
                    _ => format!("%{b:02X}"),
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// A fixture wired for both the database and a storage bucket.
fn prepared(name: &str, base: &str, url: &str, key: &str, bucket: &str) -> Fixture {
    let f = Fixture::new(name, base, SCHEMA);
    f.sql_src("INSERT INTO notes VALUES (1, 'hello')").unwrap();

    // Storage credentials sit in the same .env as the database URL, which is the
    // arrangement the tool is designed around.
    for (env, db_url) in [(".env.src", f.src_url()), (".env.dst", f.dst_url())] {
        std::fs::write(
            f.path().join(env),
            format!("DATABASE_URL={db_url}\nSTORAGE_URL={url}\nSTORAGE_KEY={key}\n"),
        )
        .unwrap();
    }
    // The config is written by hand rather than via write_config, which does not
    // know about storage.
    std::fs::write(
        f.path().join("seedle.yaml"),
        format!(
            "version: 1\n\
             sources:\n  \
               src:\n    engine: postgres\n    env_file: .env.src\n    default: true\n    \
                 storage:\n      url_var: STORAGE_URL\n      key_var: STORAGE_KEY\n  \
               dst:\n    engine: postgres\n    env_file: .env.dst\n    \
                 storage:\n      url_var: STORAGE_URL\n      key_var: STORAGE_KEY\n\
             tables:\n  notes: {{}}\n\
             buckets:\n  {bucket}: {{}}\n"
        ),
    )
    .unwrap();
    f
}

#[test]
fn objects_export_with_a_hash_and_reload_intact() {
    let base = require_pg!();
    let (url, key) = require_storage!();
    let s = Storage::new(&url, &key, "seedle_rt");
    let f = prepared("bucket_rt", &base, &url, &key, "seedle_rt");

    // A spread of shapes: nested keys, spaces and parentheses, raw binary, an
    // empty object, and non-UTF-8 content under an ASCII key.
    //
    // Non-ASCII *keys* are deliberately absent: Supabase Storage rejects them
    // with a 400, so they cannot occur in a real bucket. Non-ASCII *content* is
    // covered by the accented text below.
    let cases: &[(&str, &[u8])] = &[
        ("plain.txt", b"hello"),
        ("nested/deeper/file.bin", &[0u8, 1, 2, 255, 254]),
        (
            "with space (1).txt",
            "accented content é and emoji 🌱".as_bytes(),
        ),
        ("empty.txt", b""),
    ];
    for (k, v) in cases {
        s.put(k, v);
    }

    f.ok(&["lock", "-q"]);
    f.ok(&["export", "-q"]);

    // The manifest records every object with a sha256.
    let manifest = f.read_seed("buckets/seedle_rt/manifest.jsonl");
    assert_eq!(
        manifest.lines().count(),
        cases.len(),
        "manifest:\n{manifest}"
    );
    for (k, v) in cases {
        assert!(
            manifest.contains(k),
            "{k} missing from manifest:\n{manifest}"
        );
        // The recorded hash must be the real sha256 of the real bytes.
        let expected = sha256_hex(v);
        assert!(
            manifest.contains(&expected),
            "sha256 of {k} ({expected}) not in manifest:\n{manifest}"
        );
    }

    // verify is offline and must pass.
    f.ok(&["verify"]).says("bucket object");

    // Wipe the bucket, load it back, and confirm every byte returned.
    for (k, _) in cases {
        s.delete(k);
    }
    f.ok(&["load", "--yes", "-q"]);
    for (k, v) in cases {
        assert_eq!(s.get(k), *v, "{k} did not survive the round trip");
    }

    // And a re-export is byte-identical.
    f.ok(&["export", "-o", "out-b", "-q"]);
    if let Some(d) = dirs_differ(
        &f.seed_dir().join("buckets"),
        &f.path().join("out-b/buckets"),
    ) {
        panic!("bucket export is not reproducible:\n{d}");
    }
}

#[test]
fn a_hand_edited_object_is_caught_offline() {
    let base = require_pg!();
    let (url, key) = require_storage!();
    let s = Storage::new(&url, &key, "seedle_tamper");
    let f = prepared("bucket_tamper", &base, &url, &key, "seedle_tamper");
    s.put("doc.txt", b"original");

    f.ok(&["lock", "-q"]);
    f.ok(&["export", "-q"]);
    f.ok(&["verify"]);

    // Change a byte on disk; the recorded hash no longer matches.
    let path = f.seed_dir().join("buckets/seedle_tamper/objects/doc.txt");
    std::fs::write(&path, b"tampered").unwrap();

    let r = f.fail(&["verify"]);
    r.says("doc.txt");
    r.says("changed since export");

    // And a load must refuse to push the corrupted file.
    let bad = f.fail(&["load", "--yes"]);
    bad.says("does not match its recorded hash");
    assert_eq!(
        s.get("doc.txt"),
        b"original",
        "a corrupted local file must not reach storage"
    );
}

#[test]
fn a_removed_object_disappears_from_the_export() {
    let base = require_pg!();
    let (url, key) = require_storage!();
    let s = Storage::new(&url, &key, "seedle_removed");
    let f = prepared("bucket_removed", &base, &url, &key, "seedle_removed");
    s.put("keep.txt", b"keep");
    s.put("drop.txt", b"drop");

    f.ok(&["lock", "-q"]);
    f.ok(&["export", "-q"]);
    assert_eq!(
        f.read_seed("buckets/seedle_removed/manifest.jsonl")
            .lines()
            .count(),
        2
    );

    s.delete("drop.txt");
    f.ok(&["export", "-q"]);

    let manifest = f.read_seed("buckets/seedle_removed/manifest.jsonl");
    assert_eq!(manifest.lines().count(), 1, "{manifest}");
    assert!(manifest.contains("keep.txt"));
    // The stale local file must be gone too, or a later load would resurrect it.
    assert!(
        !f.seed_dir()
            .join("buckets/seedle_removed/objects/drop.txt")
            .exists(),
        "a deleted object left its bytes behind, so a load would put it back"
    );
    f.ok(&["verify"]);
}

#[test]
fn a_missing_bucket_is_an_error_not_something_seedle_creates() {
    let base = require_pg!();
    let (url, key) = require_storage!();
    let s = Storage::new(&url, &key, "seedle_create");
    let f = prepared("bucket_create", &base, &url, &key, "seedle_create");
    s.put("a.txt", b"a");

    f.ok(&["lock", "-q"]);
    f.ok(&["export", "-q"]);

    // Buckets are created by migrations. seedle moves data and must never
    // create one behind the user's back with settings guessed from a seed file.
    s.delete_bucket();
    let r = f.fail(&["load", "--yes"]);
    r.says("bucket does not exist");
    r.says("migrations");
    r.does_not_say("created bucket");
}

#[test]
fn changed_bucket_settings_are_reported_as_drift() {
    let base = require_pg!();
    let (url, key) = require_storage!();
    let s = Storage::new(&url, &key, "seedle_settings");
    let f = prepared("bucket_settings", &base, &url, &key, "seedle_settings");
    s.put("a.txt", b"hello");

    f.ok(&["lock", "-q"]);
    f.ok(&["export", "-q"]);

    // Flipping the bucket public does not stop an upload, so it is benign.
    s.set_public(true);
    // Not -q: drift notes are warnings, which quiet suppresses.
    f.ok(&["export"]).says("public");
}

#[test]
fn no_buckets_skips_storage_entirely() {
    let base = require_pg!();
    let (url, key) = require_storage!();
    let s = Storage::new(&url, &key, "seedle_skip");
    let f = prepared("bucket_skip", &base, &url, &key, "seedle_skip");
    s.put("a.txt", b"a");

    f.ok(&["lock", "-q"]);
    f.ok(&["export", "--no-buckets", "-q"]);
    assert!(
        !f.seed_dir().join("buckets").exists(),
        "--no-buckets still wrote a buckets directory"
    );

    // And the lock records no buckets, so verify has nothing to check.
    f.ok(&["verify"]).does_not_say("bucket object");
}

#[test]
fn an_object_over_the_size_limit_is_refused_with_the_knob_named() {
    let base = require_pg!();
    let (url, key) = require_storage!();
    let s = Storage::new(&url, &key, "seedle_big");
    let f = prepared("bucket_big", &base, &url, &key, "seedle_big");
    s.put("big.bin", &vec![7u8; 4096]);

    // Set the cap below the object's size.
    let cfg = f.path().join("seedle.yaml");
    let text = std::fs::read_to_string(&cfg).unwrap().replace(
        "  seedle_big: {}",
        "  seedle_big:\n    max_object_bytes: 1024",
    );
    std::fs::write(&cfg, text).unwrap();

    f.ok(&["lock", "-q"]);
    let r = f.fail(&["export"]);
    r.says("big.bin");
    r.says("max_object_bytes");
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

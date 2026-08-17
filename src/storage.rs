//! Object storage buckets: export, load, and hashing.
//!
//! Buckets are the half of a Supabase project that a SQL dump cannot capture.
//! The `storage.objects` table records metadata, but the bytes live in the
//! storage service, so seedle talks to the Storage REST API — which also means
//! the same code works against a local stack and a hosted project.
//!
//! What lands on disk, per bucket:
//!
//! ```text
//! seed_data/buckets/<bucket>/
//!   manifest.jsonl     one line per object, sorted by path
//!   objects/<key>      the bytes, mirroring the object key
//! ```
//!
//! The manifest carries a sha256 per object, and the lock records a single hash
//! over the manifest — so one value in `seedle.lock` covers every byte in the
//! bucket, and `seedle verify` can re-check it all without a network call.
//!
//! A bucket's *settings* — public, size limit, allowed mime types — are schema,
//! created and changed by migrations. They are recorded in `seedle.lock` as a
//! contract to check against, never written as an editable file and never
//! applied: seedle moves data, it does not touch schema. A bucket that does not
//! exist in the target is an error telling you to run your migrations, not an
//! invitation to create it.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::BucketConfig;

/// Objects requested per list call. The API caps this; 1000 is its own maximum.
const LIST_PAGE: usize = 1000;

/// Refuse to export an object larger than this unless the config raises it.
/// Storage is not the right place for a multi-gigabyte fixture, and finding out
/// after a long download is worse than being told up front.
pub const DEFAULT_MAX_OBJECT_BYTES: u64 = 25 * 1024 * 1024;

/// Bucket settings, as recorded in the lock.
///
/// Only the fields that form a contract worth checking: ids and timestamps are
/// assigned by the project and would be pure diff noise.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BucketSettings {
    pub name: String,
    #[serde(default)]
    pub public: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_size_limit: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_mime_types: Option<Vec<String>>,
}

/// One object in a bucket's manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectEntry {
    /// Object key, exactly as storage reports it.
    pub path: String,
    pub size: u64,
    /// Hex sha256 of the bytes. Verifiable with `sha256sum`.
    pub sha256: String,
    /// Needed to re-upload faithfully; storage guesses otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
}

/// A bucket's *data* as it exists on disk. Settings live in the lock.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BucketExport {
    /// Sorted by path, so the manifest is byte-stable.
    pub objects: Vec<ObjectEntry>,
}

impl BucketExport {
    pub fn total_bytes(&self) -> u64 {
        self.objects.iter().map(|o| o.size).sum()
    }

    /// The manifest file's exact bytes: one JSON object per line, sorted by
    /// path, with keys in a fixed order.
    pub fn manifest_bytes(&self) -> Result<Vec<u8>> {
        let mut buf = crate::io::LineBuffer::new();
        for o in &self.objects {
            // serde_json with a struct emits fields in declaration order, which
            // is fixed at compile time — so this is stable by construction.
            buf.push_line(&serde_json::to_string(o).context("serializing a manifest entry")?);
        }
        Ok(buf.finish())
    }

    /// One hash covering the bucket's contents.
    ///
    /// Taken over the manifest rather than the file bytes, because the manifest
    /// already contains every object's own sha256 plus its path and size — so a
    /// change to any byte, name, or ordering changes this value, and computing
    /// it needs no second pass over the data.
    pub fn hash(&self) -> Result<String> {
        Ok(crate::lock::file_hash(&self.manifest_bytes()?))
    }

    /// Size of the largest object, for checking against a bucket's size limit.
    pub fn largest_object(&self) -> u64 {
        self.objects.iter().map(|o| o.size).max().unwrap_or(0)
    }

    /// Read a bucket's manifest back from disk.
    pub fn read(dir: &Path) -> Result<BucketExport> {
        let manifest_path = dir.join("manifest.jsonl");
        let text = std::fs::read_to_string(&manifest_path)
            .with_context(|| format!("reading {}", manifest_path.display()))?;

        let mut objects = Vec::new();
        for (i, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let entry: ObjectEntry = serde_json::from_str(line)
                .with_context(|| format!("parsing {}:{}", manifest_path.display(), i + 1))?;
            objects.push(entry);
        }
        Ok(BucketExport { objects })
    }

    /// Local path holding an object's bytes.
    pub fn object_path(dir: &Path, key: &str) -> Result<PathBuf> {
        Ok(dir.join("objects").join(safe_relative_path(key)?))
    }
}

/// Convert an object key into a relative path that cannot escape its directory.
///
/// Object keys are attacker-controlled in the general case: a key of
/// `../../.ssh/authorized_keys` would otherwise make an export write outside the
/// seed directory. Anything that is not a plain forward-slash-separated relative
/// path is refused rather than silently rewritten, so a surprising key is a
/// visible error instead of a file in an unexpected place.
pub fn safe_relative_path(key: &str) -> Result<PathBuf> {
    if key.is_empty() {
        bail!("object key is empty");
    }
    if key.starts_with('/') {
        bail!("object key {key:?} is absolute");
    }
    if key.contains('\0') {
        bail!("object key {key:?} contains a NUL byte");
    }
    // Windows drive letters and UNC prefixes would also escape.
    if key.contains('\\') {
        bail!("object key {key:?} contains a backslash, which is not a path separator here");
    }

    let mut out = PathBuf::new();
    for segment in key.split('/') {
        match segment {
            // A trailing slash makes an empty final segment; storage uses that
            // for folder placeholders, which have no bytes to write.
            "" => bail!("object key {key:?} has an empty path segment"),
            "." | ".." => bail!("object key {key:?} contains a {segment:?} segment"),
            s => out.push(s),
        }
    }

    // Belt and braces: reject anything the OS would still interpret specially.
    if out.components().any(|c| !matches!(c, Component::Normal(_))) {
        bail!("object key {key:?} does not resolve to a plain relative path");
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Storage API client
// ---------------------------------------------------------------------------

/// Resolved credentials for a source's storage service.
#[derive(Debug, Clone)]
pub struct StorageAccess {
    /// Base project URL, without a trailing slash (`http://127.0.0.1:54321`).
    pub base_url: String,
    /// Service-role key. Anon keys cannot list or write.
    pub key: String,
}

impl StorageAccess {
    fn storage_url(&self) -> String {
        format!("{}/storage/v1", self.base_url.trim_end_matches('/'))
    }
}

pub struct StorageClient {
    http: reqwest::Client,
    access: StorageAccess,
}

/// What the list endpoint returns per entry.
#[derive(Debug, Deserialize)]
struct ListEntry {
    name: String,
    /// `None` marks a prefix ("folder") rather than an object.
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    metadata: Option<ListMetadata>,
}

#[derive(Debug, Deserialize)]
struct ListMetadata {
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    mimetype: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BucketResponse {
    name: String,
    #[serde(default)]
    public: bool,
    #[serde(default)]
    file_size_limit: Option<u64>,
    #[serde(default)]
    allowed_mime_types: Option<Vec<String>>,
}

impl StorageClient {
    pub fn new(access: StorageAccess) -> Result<StorageClient> {
        let http = reqwest::Client::builder()
            // Storage can be slow on a cold local stack, but a hang should still
            // end in an error rather than a wedged command.
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .context("building the storage http client")?;
        Ok(StorageClient { http, access })
    }

    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.http
            .get(format!("{}{path}", self.access.storage_url()))
            .bearer_auth(&self.access.key)
    }

    /// Bucket settings, or a clear error when the bucket is absent.
    pub async fn bucket(&self, name: &str) -> Result<BucketSettings> {
        let resp = self
            .get(&format!("/bucket/{}", urlencode(name)))
            .send()
            .await
            .with_context(|| format!("requesting bucket {name:?}"))?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            bail!(
                "bucket {name:?} does not exist in this project.\n\
                 Create it first, or remove it from the `buckets:` section of the config."
            );
        }
        let b: BucketResponse = json_or_error(resp, &format!("bucket {name:?}")).await?;
        Ok(BucketSettings {
            name: b.name,
            public: b.public,
            file_size_limit: b.file_size_limit,
            allowed_mime_types: b.allowed_mime_types.filter(|v| !v.is_empty()),
        })
    }

    /// Every object under `prefix`, recursively.
    ///
    /// The list endpoint is one level deep and pages, so this walks prefixes
    /// itself. Results come back sorted by path, which is what makes the
    /// manifest deterministic regardless of the order the API happened to use.
    pub async fn list_objects(&self, bucket: &str, prefix: &str) -> Result<Vec<ObjectEntry>> {
        let mut found: BTreeMap<String, ObjectEntry> = BTreeMap::new();
        let mut queue = vec![prefix.to_string()];

        while let Some(current) = queue.pop() {
            let mut offset = 0usize;
            loop {
                let body = serde_json::json!({
                    "prefix": current,
                    "limit": LIST_PAGE,
                    "offset": offset,
                    // Ask for a stable order too, so paging cannot drop or
                    // duplicate an entry between calls.
                    "sortBy": {"column": "name", "order": "asc"},
                });
                let resp = self
                    .http
                    .post(format!(
                        "{}/object/list/{}",
                        self.access.storage_url(),
                        urlencode(bucket)
                    ))
                    .bearer_auth(&self.access.key)
                    .json(&body)
                    .send()
                    .await
                    .with_context(|| format!("listing {bucket}/{current}"))?;

                let entries: Vec<ListEntry> =
                    json_or_error(resp, &format!("listing {bucket}/{current}")).await?;
                let page_len = entries.len();

                for e in entries {
                    let key = if current.is_empty() {
                        e.name.clone()
                    } else {
                        format!("{}/{}", current.trim_end_matches('/'), e.name)
                    };
                    match e.id {
                        // A null id means this is a prefix, not an object.
                        None => queue.push(key),
                        Some(_) => {
                            // Storage creates this marker to keep an empty
                            // folder visible; it is not real content.
                            if e.name == ".emptyFolderPlaceholder" {
                                continue;
                            }
                            found.insert(
                                key.clone(),
                                ObjectEntry {
                                    path: key,
                                    size: e.metadata.as_ref().and_then(|m| m.size).unwrap_or(0),
                                    // Filled in by the download, which is the
                                    // only place the bytes are actually seen.
                                    sha256: String::new(),
                                    content_type: e
                                        .metadata
                                        .as_ref()
                                        .and_then(|m| m.mimetype.clone())
                                        .filter(|m| !m.is_empty()),
                                },
                            );
                        }
                    }
                }

                if page_len < LIST_PAGE {
                    break;
                }
                offset += page_len;
            }
        }

        Ok(found.into_values().collect())
    }

    /// Download one object's bytes.
    pub async fn download(&self, bucket: &str, key: &str) -> Result<Vec<u8>> {
        let resp = self
            .get(&format!(
                "/object/{}/{}",
                urlencode(bucket),
                urlencode_path(key)
            ))
            .send()
            .await
            .with_context(|| format!("downloading {bucket}/{key}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!(
                "downloading {bucket}/{key} failed with {status}: {}",
                truncate(&body)
            );
        }
        Ok(resp
            .bytes()
            .await
            .with_context(|| format!("reading the body of {bucket}/{key}"))?
            .to_vec())
    }

    /// Upload one object, replacing any existing one at that key.
    pub async fn upload(
        &self,
        bucket: &str,
        key: &str,
        bytes: Vec<u8>,
        content_type: Option<&str>,
    ) -> Result<()> {
        let mut req = self
            .http
            .post(format!(
                "{}/object/{}/{}",
                self.access.storage_url(),
                urlencode(bucket),
                urlencode_path(key)
            ))
            .bearer_auth(&self.access.key)
            // Without this an existing object is a 409 rather than a replace,
            // which would make a second load fail instead of converging.
            .header("x-upsert", "true")
            .body(bytes);
        if let Some(ct) = content_type {
            req = req.header(reqwest::header::CONTENT_TYPE, ct);
        }

        let resp = req
            .send()
            .await
            .with_context(|| format!("uploading {bucket}/{key}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!(
                "uploading {bucket}/{key} failed with {status}: {}",
                truncate(&body)
            );
        }
        Ok(())
    }

    /// Cheap reachability probe, for `seedle sources`.
    pub async fn ping(&self) -> Result<usize> {
        let resp = self
            .get("/bucket")
            .send()
            .await
            .context("listing buckets")?;
        let buckets: Vec<BucketResponse> = json_or_error(resp, "listing buckets").await?;
        Ok(buckets.len())
    }
}

/// Export one bucket: list it, download every object, hash as we go.
///
/// Returns the settings alongside the data so the caller can record them in the
/// lock and compare them for drift; they are never written to the seed tree.
pub async fn export_bucket(
    client: &StorageClient,
    name: &str,
    cfg: &BucketConfig,
) -> Result<(BucketSettings, BucketExport, Vec<(String, Vec<u8>)>)> {
    let settings = client.bucket(name).await?;
    let prefix = cfg.prefix.clone().unwrap_or_default();
    let listed = client
        .list_objects(name, prefix.trim_start_matches('/'))
        .await?;

    let max = cfg.max_object_bytes.unwrap_or(DEFAULT_MAX_OBJECT_BYTES);
    let mut objects = Vec::with_capacity(listed.len());
    let mut blobs = Vec::with_capacity(listed.len());

    for mut entry in listed {
        // Validate before any bytes move, so a hostile key cannot cause a write
        // outside the seed directory even transiently.
        safe_relative_path(&entry.path)
            .with_context(|| format!("bucket {name}: refusing object key"))?;

        if entry.size > max {
            bail!(
                "bucket {name}: object {:?} is {} bytes, over the {max}-byte limit.\n\
                 Raise `max_object_bytes` for this bucket, or narrow it with `prefix`.",
                entry.path,
                entry.size
            );
        }

        let bytes = client.download(name, &entry.path).await?;
        // Trust the bytes over the metadata: size is the listing's claim, this
        // is what actually arrived.
        entry.size = bytes.len() as u64;
        entry.sha256 = crate::lock::file_hash(&bytes);
        blobs.push((entry.path.clone(), bytes));
        objects.push(entry);
    }

    objects.sort_by(|a, b| a.path.cmp(&b.path));
    Ok((settings, BucketExport { objects }, blobs))
}

/// Percent-encode one path segment.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Percent-encode an object key, keeping `/` as a separator.
fn urlencode_path(s: &str) -> String {
    s.split('/').map(urlencode).collect::<Vec<_>>().join("/")
}

/// Decode a JSON body, or turn a non-success status into a useful error.
async fn json_or_error<T: serde::de::DeserializeOwned>(
    resp: reqwest::Response,
    what: &str,
) -> Result<T> {
    let status = resp.status();
    let body = resp
        .text()
        .await
        .with_context(|| format!("reading the response to {what}"))?;

    if !status.is_success() {
        // 401/403 here almost always means an anon key was used.
        let hint = if status == reqwest::StatusCode::UNAUTHORIZED
            || status == reqwest::StatusCode::FORBIDDEN
        {
            "\nStorage listing and writing need the service-role key, not the anon key."
        } else {
            ""
        };
        bail!("{what} failed with {status}: {}{hint}", truncate(&body));
    }
    serde_json::from_str(&body)
        .with_context(|| format!("parsing the response to {what}: {}", truncate(&body)))
}

fn truncate(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() <= 300 {
        return s.to_string();
    }
    format!("{}…", s.chars().take(300).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, size: u64, sha: &str) -> ObjectEntry {
        ObjectEntry {
            path: path.into(),
            size,
            sha256: sha.into(),
            content_type: Some("application/pdf".into()),
        }
    }

    fn export(objects: Vec<ObjectEntry>) -> BucketExport {
        BucketExport { objects }
    }

    // -- path safety --------------------------------------------------------

    #[test]
    fn ordinary_keys_become_relative_paths() {
        assert_eq!(safe_relative_path("a.pdf").unwrap(), PathBuf::from("a.pdf"));
        assert_eq!(
            safe_relative_path("uuid/nested/file.pdf").unwrap(),
            PathBuf::from("uuid/nested/file.pdf")
        );
        // Spaces, unicode, and dots inside a segment are all fine.
        assert_eq!(
            safe_relative_path("my folder/rapport final é.pdf").unwrap(),
            PathBuf::from("my folder/rapport final é.pdf")
        );
        assert_eq!(
            safe_relative_path("v1.2.3/a..b").unwrap(),
            PathBuf::from("v1.2.3/a..b")
        );
    }

    #[test]
    fn traversal_keys_are_refused_not_rewritten() {
        // An object key is attacker-controlled in the general case; silently
        // sanitising one would put a file somewhere the user did not expect.
        for key in [
            "../etc/passwd",
            "a/../../b",
            "..",
            "./a",
            "a/./b",
            "/absolute",
            "a//b",
            "a/",
            "",
            "back\\slash",
        ] {
            assert!(
                safe_relative_path(key).is_err(),
                "{key:?} should be refused"
            );
        }
    }

    #[test]
    fn a_nul_byte_in_a_key_is_refused() {
        assert!(safe_relative_path("a\0b").is_err());
    }

    #[test]
    fn refused_keys_name_themselves_in_the_error() {
        let err = safe_relative_path("../secret").unwrap_err().to_string();
        assert!(err.contains("../secret"), "{err}");
    }

    // -- manifest determinism ----------------------------------------------

    #[test]
    fn the_manifest_is_one_sorted_line_per_object() {
        let e = export(vec![entry("a.pdf", 3, "aa"), entry("b.pdf", 4, "bb")]);
        let text = String::from_utf8(e.manifest_bytes().unwrap()).unwrap();
        assert_eq!(
            text,
            "{\"path\":\"a.pdf\",\"size\":3,\"sha256\":\"aa\",\"content_type\":\"application/pdf\"}\n\
             {\"path\":\"b.pdf\",\"size\":4,\"sha256\":\"bb\",\"content_type\":\"application/pdf\"}\n"
        );
    }

    #[test]
    fn manifest_keys_are_in_a_fixed_order() {
        // Field order comes from the struct declaration, so it cannot drift with
        // a map's iteration order.
        let text =
            String::from_utf8(export(vec![entry("a", 1, "h")]).manifest_bytes().unwrap()).unwrap();
        let path_at = text.find("\"path\"").unwrap();
        let size_at = text.find("\"size\"").unwrap();
        let sha_at = text.find("\"sha256\"").unwrap();
        assert!(path_at < size_at && size_at < sha_at, "{text}");
    }

    #[test]
    fn an_empty_bucket_yields_an_empty_manifest() {
        let e = export(vec![]);
        assert!(e.manifest_bytes().unwrap().is_empty());
        assert_eq!(e.total_bytes(), 0);
        // And still has a stable hash, so drift on an empty bucket is detectable.
        assert!(!e.hash().unwrap().is_empty());
    }

    #[test]
    fn a_missing_content_type_is_omitted_rather_than_written_as_null() {
        let mut o = entry("a", 1, "h");
        o.content_type = None;
        let text = String::from_utf8(export(vec![o]).manifest_bytes().unwrap()).unwrap();
        assert_eq!(text, "{\"path\":\"a\",\"size\":1,\"sha256\":\"h\"}\n");
    }

    // -- hashing ------------------------------------------------------------

    #[test]
    fn the_bucket_hash_covers_content_and_names() {
        let base = export(vec![entry("a.pdf", 3, "aa"), entry("b.pdf", 4, "bb")]);
        let h = base.hash().unwrap();

        // Same inputs, same hash.
        assert_eq!(
            h,
            export(vec![entry("a.pdf", 3, "aa"), entry("b.pdf", 4, "bb")])
                .hash()
                .unwrap()
        );

        // A changed byte changes the object's sha, so it changes the bucket hash.
        let mut changed = base.clone();
        changed.objects[0].sha256 = "cc".into();
        assert_ne!(h, changed.hash().unwrap(), "content change must show");

        // A rename changes it too, even with identical bytes.
        let mut renamed = base.clone();
        renamed.objects[0].path = "renamed.pdf".into();
        assert_ne!(h, renamed.hash().unwrap(), "a rename must show");

        // And a removal.
        let mut fewer = base.clone();
        fewer.objects.pop();
        assert_ne!(h, fewer.hash().unwrap(), "a deletion must show");
    }

    #[test]
    fn object_hashes_are_plain_sha256() {
        // So a user can check one with the sha256sum already on their machine.
        assert_eq!(
            crate::lock::file_hash(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    // -- disk round trip ----------------------------------------------------

    #[test]
    fn a_bucket_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let e = export(vec![entry("a.pdf", 3, "aa"), entry("dir/b.pdf", 4, "bb")]);

        crate::io::write_atomic(
            &dir.path().join("manifest.jsonl"),
            &e.manifest_bytes().unwrap(),
        )
        .unwrap();

        let back = BucketExport::read(dir.path()).unwrap();
        assert_eq!(back, e);
        assert_eq!(back.hash().unwrap(), e.hash().unwrap());
    }

    #[test]
    fn object_paths_land_under_the_objects_directory() {
        let dir = Path::new("/seed/buckets/project_files");
        assert_eq!(
            BucketExport::object_path(dir, "uuid/a.pdf").unwrap(),
            PathBuf::from("/seed/buckets/project_files/objects/uuid/a.pdf")
        );
        // And a traversal key cannot produce a path outside it.
        assert!(BucketExport::object_path(dir, "../../escape").is_err());
    }

    // -- url encoding -------------------------------------------------------

    #[test]
    fn segments_are_percent_encoded() {
        assert_eq!(urlencode("plain"), "plain");
        assert_eq!(urlencode("with space"), "with%20space");
        assert_eq!(urlencode("a/b"), "a%2Fb");
        assert_eq!(urlencode("é"), "%C3%A9");
        assert_eq!(urlencode("a+b&c=d"), "a%2Bb%26c%3Dd");
    }

    #[test]
    fn object_keys_keep_their_separators() {
        assert_eq!(urlencode_path("a/b c/d.pdf"), "a/b%20c/d.pdf");
        // A query-string character in a key must not become part of the query.
        assert_eq!(urlencode_path("a?b=1"), "a%3Fb%3D1");
        assert_eq!(urlencode_path("a#b"), "a%23b");
    }
}

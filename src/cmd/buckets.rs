//! Storage bucket export and load.

use std::path::Path;

use anyhow::{Context, Result, bail};
use indexmap::IndexMap;

use crate::commands::Ctx;
use crate::lock::BucketEntry;
use crate::source::ResolvedSource;
use crate::storage::{self, BucketExport, StorageClient};

/// Live settings for every configured bucket, for drift comparison.
///
/// Returns an empty map when the config declares no buckets, so a project
/// without storage never pays for a network call.
pub async fn live_bucket_settings(
    ctx: &Ctx,
    src: &ResolvedSource,
    only: Option<&[String]>,
) -> Result<IndexMap<String, storage::BucketSettings>> {
    let selected = ctx.cfg.select_buckets(only)?;
    let mut out = IndexMap::new();
    if selected.is_empty() {
        return Ok(out);
    }
    let client = storage_client(src)?;
    for (name, _) in &selected {
        // A missing bucket is reported as drift by the caller, which explains it
        // far better than a bare request failure.
        if let Ok(settings) = client.bucket(name).await {
            out.insert(name.clone(), settings);
        }
    }
    Ok(out)
}

/// Storage client for a source, or a clear error explaining what is missing.
pub fn storage_client(src: &ResolvedSource) -> Result<StorageClient> {
    let access = src.storage.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "source {:?} has no `storage:` block, so its buckets cannot be reached.\n\
             Add `storage: {{url_var: SUPABASE_URL, key_var: SUPABASE_SERVICE_ROLE_KEY}}` to it.",
            src.name
        )
    })?;
    StorageClient::new(access)
}

/// Export every selected bucket into `out_dir/buckets/<name>/`.
type BucketExportResult = (
    IndexMap<String, storage::BucketSettings>,
    IndexMap<String, BucketEntry>,
    Vec<serde_json::Value>,
);

pub async fn export_buckets(
    ctx: &Ctx,
    src: &ResolvedSource,
    out_dir: &Path,
    only: Option<&[String]>,
) -> Result<BucketExportResult> {
    let selected = ctx.cfg.select_buckets(only)?;
    let mut settings_map = IndexMap::new();
    let mut entries = IndexMap::new();
    let mut summary = Vec::new();
    if selected.is_empty() {
        return Ok((settings_map, entries, summary));
    }

    let client = storage_client(src)?;

    for (name, cfg) in &selected {
        let (settings, export, blobs) = storage::export_bucket(&client, name, cfg)
            .await
            .with_context(|| format!("exporting bucket {name:?}"))?;

        let dir = out_dir.join("buckets").join(name);
        // Objects removed upstream must not linger locally, or a later load
        // would put them back. Rebuilding the directory is the simplest way to
        // guarantee the tree matches the manifest exactly.
        if dir.join("objects").exists() {
            std::fs::remove_dir_all(dir.join("objects"))
                .with_context(|| format!("clearing {}", dir.join("objects").display()))?;
        }
        for (key, bytes) in &blobs {
            let path = BucketExport::object_path(&dir, key)?;
            crate::io::write_atomic(&path, bytes)?;
        }
        crate::io::write_atomic(&dir.join("manifest.jsonl"), &export.manifest_bytes()?)?;

        let bytes = export.total_bytes();
        ctx.detail(format!(
            "  bucket {name}: {} object(s), {}",
            export.objects.len(),
            human_bytes(bytes)
        ));
        summary.push(serde_json::json!({
            "bucket": name,
            "objects": export.objects.len(),
            "bytes": bytes,
        }));
        entries.insert(
            name.clone(),
            BucketEntry {
                objects: export.objects.len() as u64,
                bytes,
                sha256: export.hash()?,
            },
        );
        settings_map.insert(name.clone(), settings);
    }
    Ok((settings_map, entries, summary))
}

/// Upload every selected bucket back into a project.
///
/// Deliberately not transactional: object storage has no transaction to join.
/// Uploads are upserts, so a re-run after a failure converges rather than
/// duplicating, and the database load has already committed by this point.
pub async fn load_buckets(
    ctx: &Ctx,
    src: &ResolvedSource,
    out_dir: &Path,
    only: Option<&[String]>,
    dry_run: bool,
) -> Result<Vec<serde_json::Value>> {
    let selected = ctx.cfg.select_buckets(only)?;
    let mut summary = Vec::new();
    if selected.is_empty() {
        return Ok(summary);
    }

    let client = storage_client(src)?;

    for (name, _cfg) in &selected {
        let dir = out_dir.join("buckets").join(name);
        if !dir.is_dir() {
            bail!(
                "bucket {name:?} has no exported files at {} (run `seedle export` first)",
                dir.display()
            );
        }
        let export = BucketExport::read(&dir)?;

        // Verify before uploading: a corrupted local file should not be pushed.
        let mut payloads = Vec::with_capacity(export.objects.len());
        for o in &export.objects {
            let path = BucketExport::object_path(&dir, &o.path)?;
            let bytes =
                std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            let actual = crate::lock::file_hash(&bytes);
            if actual != o.sha256 {
                bail!(
                    "bucket {name:?}: {} does not match its recorded hash.\n                       manifest: {}\n  on disk:  {actual}\n                     Re-export the bucket, or restore the file.",
                    path.display(),
                    o.sha256
                );
            }
            payloads.push((o.path.clone(), bytes, o.content_type.clone()));
        }

        if dry_run {
            ctx.say(format!(
                "would upload {} object(s) to bucket {name}",
                payloads.len()
            ));
            summary.push(serde_json::json!({
                "bucket": name, "objects": payloads.len(), "uploaded": 0, "dry_run": true,
            }));
            continue;
        }

        // Buckets are created by migrations. seedle moves data and never
        // touches schema, so a missing bucket is an error rather than something
        // to silently create with settings it guessed from a seed file.
        client.bucket(name).await.with_context(|| {
            format!(
                "bucket {name:?} must exist before its objects can be loaded, buckets are \
                 created by migrations, so run them against this project first"
            )
        })?;

        for (key, bytes, content_type) in payloads {
            client
                .upload(name, &key, bytes, content_type.as_deref())
                .await?;
        }
        ctx.detail(format!(
            "  bucket {name}: {} object(s) uploaded",
            export.objects.len()
        ));
        summary.push(serde_json::json!({
            "bucket": name,
            "objects": export.objects.len(),
            "uploaded": export.objects.len(),
        }));
    }
    Ok(summary)
}

/// Byte count in the largest unit that keeps it readable.
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

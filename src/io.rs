//! File writing with the byte-level guarantees the determinism rules require.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};

/// Write `bytes` to `path` atomically.
///
/// Writes a sibling temp file and renames it, so an interrupted export never
/// leaves a half-written seed file that a later `verify` would report as
/// corruption.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut f = AtomicFile::create(path)?;
    f.write_all(bytes)
        .with_context(|| format!("writing {}", path.display()))?;
    f.commit()?;
    Ok(())
}

/// A file that only replaces its target once it is complete.
///
/// Writes go to a sibling temp file; [`AtomicFile::commit`] syncs and renames.
/// Dropping without committing discards the temp file, so an error mid-stream
/// leaves whatever was already on disk untouched.
pub struct AtomicFile {
    target: std::path::PathBuf,
    tmp: std::path::PathBuf,
    file: Option<std::io::BufWriter<std::fs::File>>,
    hasher: sha2::Sha256,
}

impl AtomicFile {
    pub fn create(path: &Path) -> Result<AtomicFile> {
        let dir = path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "graine".to_string());
        let tmp = dir.join(format!(".{file_name}.tmp"));
        let file =
            std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        Ok(AtomicFile {
            target: path.to_path_buf(),
            tmp,
            file: Some(std::io::BufWriter::with_capacity(64 * 1024, file)),
            hasher: <sha2::Sha256 as sha2::Digest>::new(),
        })
    }

    /// Mix bytes into the hash without writing them, so a content hash can
    /// cover something beyond the file's own contents.
    pub fn hash_prefix(&mut self, bytes: &[u8]) {
        sha2::Digest::update(&mut self.hasher, bytes);
    }

    /// Sync, rename, and return the hex sha256 of everything written.
    pub fn commit(mut self) -> Result<String> {
        let buf = self.file.take().expect("commit runs once");
        // `into_inner` is what flushes, and what reports a flush failure;
        // leaving it to Drop would lose the tail of the file silently.
        let f = buf
            .into_inner()
            .with_context(|| format!("flushing {}", self.tmp.display()))?;
        f.sync_all()
            .with_context(|| format!("syncing {}", self.tmp.display()))?;
        std::fs::rename(&self.tmp, &self.target).with_context(|| {
            format!(
                "replacing {} with {}",
                self.target.display(),
                self.tmp.display()
            )
        })?;
        Ok(format!(
            "{:x}",
            <sha2::Sha256 as sha2::Digest>::finalize(std::mem::replace(
                &mut self.hasher,
                <sha2::Sha256 as sha2::Digest>::new()
            ))
        ))
    }
}

impl Write for AtomicFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self
            .file
            .as_mut()
            .expect("writes precede commit")
            .write(buf)?;
        sha2::Digest::update(&mut self.hasher, &buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.as_mut().expect("writes precede commit").flush()
    }
}

impl Drop for AtomicFile {
    fn drop(&mut self) {
        if self.file.is_some() {
            let _ = std::fs::remove_file(&self.tmp);
        }
    }
}

/// Enforces the on-disk byte rules while writing straight through to a sink.
///
/// The streaming counterpart of [`LineBuffer`], with the same guarantees: LF
/// only, a trailing newline on non-empty content, and nothing at all for an
/// empty table.
pub struct LineSink<W: Write> {
    inner: W,
    wrote_any: bool,
    ends_with_newline: bool,
}

impl<W: Write> LineSink<W> {
    pub fn new(inner: W) -> Self {
        LineSink {
            inner,
            wrote_any: false,
            ends_with_newline: false,
        }
    }

    pub fn push_line(&mut self, line: &str) -> Result<()> {
        self.inner.write_all(line.as_bytes())?;
        self.inner.write_all(b"\n")?;
        self.wrote_any = true;
        self.ends_with_newline = true;
        Ok(())
    }

    pub fn push_str(&mut self, s: &str) -> Result<()> {
        if s.is_empty() {
            return Ok(());
        }
        self.inner.write_all(s.as_bytes())?;
        self.wrote_any = true;
        self.ends_with_newline = s.ends_with('\n');
        Ok(())
    }

    /// Finish, guaranteeing a trailing newline on non-empty content.
    pub fn finish(mut self) -> Result<W> {
        if self.wrote_any && !self.ends_with_newline {
            self.inner.write_all(b"\n")?;
        }
        Ok(self.inner)
    }
}

/// Buffer that enforces the on-disk byte rules: UTF-8, no BOM, LF only, and a
/// trailing newline.
#[derive(Default)]
pub struct LineBuffer {
    inner: String,
}

impl LineBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append one line, terminated by LF.
    pub fn push_line(&mut self, line: &str) {
        self.inner.push_str(line);
        self.inner.push('\n');
    }

    pub fn push_str(&mut self, s: &str) {
        self.inner.push_str(s);
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Finish, guaranteeing a trailing newline on non-empty content.
    pub fn finish(mut self) -> Vec<u8> {
        if !self.inner.is_empty() && !self.inner.ends_with('\n') {
            self.inner.push('\n');
        }
        self.inner.into_bytes()
    }
}

/// Compress with a fixed configuration, so the same input always produces the
/// same bytes.
///
/// gzip records a modification time and an OS byte in its header; both are left
/// zeroed, or two runs over identical data would differ.
pub fn gzip(bytes: &[u8]) -> Result<Vec<u8>> {
    use flate2::{Compression, GzBuilder};
    let mut out = Vec::new();
    {
        let mut w = GzBuilder::new()
            .mtime(0)
            .operating_system(255)
            .write(&mut out, Compression::new(6));
        w.write_all(bytes).context("compressing")?;
        w.finish().context("finishing the gzip stream")?;
    }
    Ok(out)
}

/// A streaming gzip encoder pinned to the same settings as [`gzip`].
///
/// Never call `flush()` on the result: flate2 issues a `Z_SYNC_FLUSH`, which
/// injects an empty stored block and changes the bytes. Only `finish()`.
pub fn gzip_encoder<W: Write>(w: W) -> flate2::write::GzEncoder<W> {
    use flate2::{Compression, GzBuilder};
    GzBuilder::new()
        .mtime(0)
        .operating_system(255)
        .write(w, Compression::new(6))
}

pub fn gunzip(bytes: &[u8]) -> Result<Vec<u8>> {
    use flate2::read::GzDecoder;
    use std::io::Read;
    let mut out = Vec::new();
    GzDecoder::new(bytes)
        .read_to_end(&mut out)
        .context("decompressing")?;
    Ok(out)
}

/// Remove files under `dir` that this run did not produce.
///
/// Returns the paths removed. Used so a table dropped from the config, or rows
/// removed from a per-row table, do not leave orphaned files behind. Recurses,
/// because a per-row table's files live in a directory of their own.
pub fn prune_stale(dir: &Path, keep: &[std::path::PathBuf]) -> Result<Vec<std::path::PathBuf>> {
    let keep: std::collections::BTreeSet<&Path> = keep.iter().map(|p| p.as_path()).collect();
    let mut removed = Vec::new();
    prune_dir(dir, &keep, &mut removed)?;
    Ok(removed)
}

fn prune_dir(
    dir: &Path,
    keep: &std::collections::BTreeSet<&Path>,
    removed: &mut Vec<std::path::PathBuf>,
) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("reading {}", dir.display()))?;
    entries.sort_by_key(|e| e.path());

    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            prune_dir(&path, keep, removed)?;
            continue;
        }
        if keep.contains(path.as_path()) {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // Never touch anything we did not write. A dot-prefixed `.tmp` is an
        // AtomicFile a killed run left behind.
        let is_ours = ["jsonl.gz", "jsonl", "csv", "sql", "json"]
            .iter()
            .any(|ext| name.ends_with(&format!(".{ext}")))
            || (name.starts_with('.') && name.ends_with(".tmp"));
        if !is_ours {
            continue;
        }
        std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        removed.push(path);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gzip_is_deterministic_and_round_trips() {
        let data = b"{\"id\":1}\n{\"id\":2}\n";
        let a = gzip(data).unwrap();
        // The header carries an mtime, so two runs would otherwise differ.
        assert_eq!(a, gzip(data).unwrap(), "compression must be reproducible");
        assert_eq!(gunzip(&a).unwrap(), data);
        assert!(!a.is_empty());
    }

    #[test]
    fn pruning_recognises_a_compressed_seed_file() {
        let dir = tempfile::tempdir().unwrap();
        write_atomic(&dir.path().join("users.jsonl.gz"), b"x").unwrap();
        write_atomic(&dir.path().join("notes.md"), b"x").unwrap();
        let removed = prune_stale(dir.path(), &[]).unwrap();
        assert_eq!(removed.len(), 1);
        assert!(removed[0].ends_with("users.jsonl.gz"));
        assert!(dir.path().join("notes.md").exists());
    }

    #[test]
    fn line_buffer_guarantees_a_trailing_newline() {
        let mut b = LineBuffer::new();
        b.push_line("a");
        b.push_line("b");
        assert_eq!(b.finish(), b"a\nb\n");

        let mut b = LineBuffer::new();
        b.push_str("no newline");
        assert_eq!(b.finish(), b"no newline\n");
    }

    #[test]
    fn empty_buffer_produces_an_empty_file_not_a_blank_line() {
        // A table with zero rows must yield a zero-byte file, so its hash is
        // stable and `git diff` shows nothing rather than a phantom line.
        assert_eq!(LineBuffer::new().finish(), Vec::<u8>::new());
    }

    #[test]
    fn atomic_write_replaces_content_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("users.jsonl");
        write_atomic(&path, b"first\n").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"first\n");
        write_atomic(&path, b"second\n").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second\n");

        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with('.'))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    #[test]
    fn atomic_write_creates_missing_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/deeper/users.jsonl");
        write_atomic(&path, b"x\n").unwrap();
        assert!(path.exists());
    }

    #[test]
    fn prune_removes_orphans_but_spares_foreign_files() {
        let dir = tempfile::tempdir().unwrap();
        let keep = dir.path().join("users.jsonl");
        write_atomic(&keep, b"x\n").unwrap();
        write_atomic(&dir.path().join("orphan.jsonl"), b"x\n").unwrap();
        // Not a format we emit, so not ours to delete.
        write_atomic(&dir.path().join("graine.lock"), b"x\n").unwrap();
        write_atomic(&dir.path().join("README.md"), b"x\n").unwrap();

        let removed = prune_stale(dir.path(), std::slice::from_ref(&keep)).unwrap();
        assert_eq!(removed.len(), 1);
        assert!(removed[0].ends_with("orphan.jsonl"));
        assert!(keep.exists());
        assert!(
            dir.path().join("graine.lock").exists(),
            "the lock must survive"
        );
        assert!(
            dir.path().join("README.md").exists(),
            "foreign files must survive"
        );
    }
}

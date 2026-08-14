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
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;

    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "seedle".to_string());
    let tmp = dir.join(format!(".{file_name}.tmp"));

    {
        let mut f =
            std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        f.write_all(bytes)
            .with_context(|| format!("writing {}", tmp.display()))?;
        f.sync_all()
            .with_context(|| format!("flushing {}", tmp.display()))?;
    }

    std::fs::rename(&tmp, path)
        .with_context(|| format!("replacing {} with {}", path.display(), tmp.display()))?;
    Ok(())
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

    /// Append one line. Any CR is stripped: a `\r\n` in the source data would
    /// otherwise make output depend on the platform that produced it.
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

/// Remove files in `dir` matching `keep`'s directory that are no longer produced.
///
/// Returns the paths removed. Used so a table dropped from the config, or rows
/// removed from a per-row table, do not leave orphaned files behind.
pub fn prune_stale(dir: &Path, keep: &[std::path::PathBuf]) -> Result<Vec<std::path::PathBuf>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let keep: std::collections::BTreeSet<&Path> = keep.iter().map(|p| p.as_path()).collect();
    let mut removed = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("reading {}", dir.display()))?;
    entries.sort_by_key(|e| e.path());

    for entry in entries {
        let path = entry.path();
        if path.is_dir() || keep.contains(path.as_path()) {
            continue;
        }
        // Never touch anything we did not write.
        let is_ours = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| matches!(e, "jsonl" | "csv" | "sql" | "json"));
        if !is_ours {
            continue;
        }
        std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        removed.push(path);
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        write_atomic(&dir.path().join("seedle.lock"), b"x\n").unwrap();
        write_atomic(&dir.path().join("README.md"), b"x\n").unwrap();

        let removed = prune_stale(dir.path(), std::slice::from_ref(&keep)).unwrap();
        assert_eq!(removed.len(), 1);
        assert!(removed[0].ends_with("orphan.jsonl"));
        assert!(keep.exists());
        assert!(
            dir.path().join("seedle.lock").exists(),
            "the lock must survive"
        );
        assert!(
            dir.path().join("README.md").exists(),
            "foreign files must survive"
        );
    }
}

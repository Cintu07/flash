//! Content-addressed blob store. Write-once, blake3-keyed.

use crate::{Result, StoreError, atomic_write, ensure_dir, shard_path};
use flash_core::Digest;
use std::path::PathBuf;

pub struct ContentStore {
    root: PathBuf,
}

impl ContentStore {
    pub fn open(root: PathBuf) -> Result<Self> {
        ensure_dir(&root)?;
        Ok(ContentStore { root })
    }

    /// Store bytes, return their digest. Storing the same bytes twice is a no-op.
    pub fn put(&self, bytes: &[u8]) -> Result<Digest> {
        let d = Digest::of(bytes);
        let path = shard_path(&self.root, &d.hex(), "");
        if path.exists() {
            return Ok(d);
        }
        atomic_write(&path, bytes)?;
        Ok(d)
    }

    pub fn get(&self, d: &Digest) -> Result<Vec<u8>> {
        let path = shard_path(&self.root, &d.hex(), "");
        std::fs::read(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                StoreError::Missing(d.short())
            } else {
                StoreError::Io { path, source: e }
            }
        })
    }

    pub fn has(&self, d: &Digest) -> bool {
        shard_path(&self.root, &d.hex(), "").exists()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_is_idempotent_and_get_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let cs = ContentStore::open(dir.path().join("blobs")).unwrap();
        let a = cs.put(b"hello world").unwrap();
        let b = cs.put(b"hello world").unwrap();
        assert_eq!(a, b);
        assert_eq!(cs.get(&a).unwrap(), b"hello world");
        assert!(cs.has(&a));
    }

    #[test]
    fn missing_content_is_an_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let cs = ContentStore::open(dir.path().join("blobs")).unwrap();
        let d = Digest::of(b"never stored");
        assert!(matches!(cs.get(&d), Err(StoreError::Missing(_))));
    }
}

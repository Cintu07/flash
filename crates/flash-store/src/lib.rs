//! flash-store: everything durable.
//!
//! Four things live here, all under one root directory so a store can be tarred, shipped to a
//! teammate, or mounted as a shared team cache (d10):
//!
//! * [`ContentStore`] - blake3-addressed blobs. Write-once, so a write is never a race.
//! * [`MemoStore`] - action key to result. **Only passing results are ever written** (d4 amended:
//!   memoization is verdict-gated). Caching a failing node freezes one bad sample forever, and
//!   under a shared cache it spreads to the whole team.
//! * [`History`] - the last N wall times per *logical* node key. This is what etas come from, and
//!   it is keyed on `NodeKey` rather than `ActionKey` on purpose: a node with fresh inputs has a
//!   brand new action key every run, but its cost is a property of the step, not of the bytes.
//! * [`Journal`] - append-only record of completed nodes per task, so a crash resumes at the
//!   graph frontier instead of from zero (section 3.2).
//!
//! Everything on disk is written to a temp file and renamed, so a crash mid-write leaves either
//! the old file or the new one, never a half file.

mod cas;
mod history;
mod journal;
mod memo;
mod worktree;

pub use cas::ContentStore;
pub use history::{History, Stats};
pub use journal::{Journal, JournalEntry};
pub use memo::{MemoEntry, MemoStore};
pub use worktree::{MaterializeStats, TreeSpec, Worktree};

use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("corrupt record at {path}: {source}")]
    Corrupt {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("content {0} is not in the store")]
    Missing(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// The whole durable side of the runtime, rooted at one directory.
pub struct Store {
    pub content: ContentStore,
    pub memo: MemoStore,
    pub history: History,
    pub journal: Journal,
    root: PathBuf,
}

impl Store {
    pub fn open(root: impl AsRef<Path>) -> Result<Arc<Store>> {
        let root = root.as_ref().to_path_buf();
        Ok(Arc::new(Store {
            content: ContentStore::open(root.join("blobs"))?,
            memo: MemoStore::open(root.join("memo"))?,
            history: History::open(root.join("history"))?,
            journal: Journal::open(root.join("journal"))?,
            root,
        }))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

// ---- shared fs helpers -------------------------------------------------------------------

pub(crate) fn ensure_dir(p: &Path) -> Result<()> {
    std::fs::create_dir_all(p).map_err(|source| StoreError::Io {
        path: p.to_path_buf(),
        source,
    })
}

/// Two-level fanout so no directory holds a million entries.
pub(crate) fn shard_path(root: &Path, hex: &str, suffix: &str) -> PathBuf {
    root.join(&hex[0..2])
        .join(format!("{}{}", &hex[2..], suffix))
}

/// Write bytes so that readers see all of them or none of them.
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        ensure_dir(parent)?;
    }
    let tmp = path.with_extension(format!("tmp{}", unique_suffix()));
    std::fs::write(&tmp, bytes).map_err(|source| StoreError::Io {
        path: tmp.clone(),
        source,
    })?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Windows refuses to rename over an existing file. Write-once content makes losing
            // the race harmless: the bytes already there are the bytes we were about to write.
            let _ = std::fs::remove_file(&tmp);
            if path.exists() {
                Ok(())
            } else {
                Err(StoreError::Io {
                    path: path.to_path_buf(),
                    source: e,
                })
            }
        }
    }
}

/// Replace a file that legitimately changes over time (history, not content).
pub(crate) fn atomic_replace(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        ensure_dir(parent)?;
    }
    let tmp = path.with_extension(format!("tmp{}", unique_suffix()));
    std::fs::write(&tmp, bytes).map_err(|source| StoreError::Io {
        path: tmp.clone(),
        source,
    })?;
    if path.exists() {
        let _ = std::fs::remove_file(path);
    }
    std::fs::rename(&tmp, path).map_err(|source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn unique_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("{:x}.{:x}.{:x}", std::process::id(), t, n)
}

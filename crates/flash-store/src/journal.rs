//! Per-task append-only journal.
//!
//! Section 3.2: "every completed node is durable. a crash resumes at the graph frontier, not from
//! zero." The memo store already makes a completed node replayable; the journal is what tells a
//! resuming daemon which frontier it reached, and is the audit trail for the benchmark's per node
//! attribution (d11).

use crate::{Result, StoreError, ensure_dir};
use flash_core::{ActionKey, Attribution, Digest, NodeKey};
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JournalEntry {
    pub node_key: NodeKey,
    pub action: ActionKey,
    pub outputs: Vec<Digest>,
    /// True when this node was served from the memo store with zero work.
    pub hit: bool,
    pub duration_ms: u64,
    pub attribution: Attribution,
    pub at_unix_ms: u64,
}

pub struct Journal {
    root: PathBuf,
    lock: Mutex<()>,
}

impl Journal {
    pub fn open(root: PathBuf) -> Result<Self> {
        ensure_dir(&root)?;
        Ok(Journal {
            root,
            lock: Mutex::new(()),
        })
    }

    fn path(&self, task_id: &str) -> PathBuf {
        self.root.join(format!("{}.jsonl", sanitize(task_id)))
    }

    pub fn append(&self, task_id: &str, entry: &JournalEntry) -> Result<()> {
        let path = self.path(task_id);
        let mut line = serde_json::to_vec(entry).expect("journal entry is serializable");
        line.push(b'\n');
        let _guard = self.lock.lock().unwrap();
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|source| StoreError::Io {
                path: path.clone(),
                source,
            })?;
        f.write_all(&line).map_err(|source| StoreError::Io {
            path: path.clone(),
            source,
        })?;
        f.flush().map_err(|source| StoreError::Io { path, source })
    }

    /// Everything already completed for this task. A resuming run skips these.
    pub fn replay(&self, task_id: &str) -> Result<Vec<JournalEntry>> {
        let path = self.path(task_id);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => return Err(StoreError::Io { path, source }),
        };
        let text = String::from_utf8_lossy(&bytes);
        let mut out = Vec::new();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            // A torn last line after a crash is expected; stop there rather than failing the run.
            match serde_json::from_str::<JournalEntry>(line) {
                Ok(e) => out.push(e),
                Err(_) => break,
            }
        }
        Ok(out)
    }

    pub fn clear(&self, task_id: &str) -> Result<()> {
        let path = self.path(task_id);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(StoreError::Io { path, source }),
        }
    }
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memo::now_ms;

    fn entry(k: &str) -> JournalEntry {
        JournalEntry {
            node_key: NodeKey::new(k),
            action: ActionKey(Digest::of(k.as_bytes())),
            outputs: vec![Digest::of(b"o")],
            hit: false,
            duration_ms: 5,
            attribution: Attribution::default(),
            at_unix_ms: now_ms(),
        }
    }

    #[test]
    fn replay_returns_completed_nodes_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::open(dir.path().join("j")).unwrap();
        j.append("task-1", &entry("a")).unwrap();
        j.append("task-1", &entry("b")).unwrap();
        let got = j.replay("task-1").unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].node_key.as_str(), "a");
        assert_eq!(got[1].node_key.as_str(), "b");
    }

    #[test]
    fn a_torn_trailing_line_does_not_fail_the_replay() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::open(dir.path().join("j")).unwrap();
        j.append("t", &entry("a")).unwrap();
        let path = dir.path().join("j").join("t.jsonl");
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{\"node_key\":\"trunc").unwrap();
        assert_eq!(j.replay("t").unwrap().len(), 1);
    }

    #[test]
    fn unknown_task_replays_empty() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::open(dir.path().join("j")).unwrap();
        assert!(j.replay("never-ran").unwrap().is_empty());
    }
}

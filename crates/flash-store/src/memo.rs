//! The memo store: action key to result.
//!
//! One rule, and it is load bearing: **only passing results are written**.
//!
//! The PRD's d4 says "result cached by node id". For deterministic nodes that is fine. For an
//! llm node it is not: the output is one sample from a distribution, and a memo write makes that
//! one sample permanent for every future run with the same inputs, on every machine sharing the
//! cache. If the sample was wrong, the cache has now made a transient failure into a durable one.
//! So a node is only memoized once its verdict is Pass. A failure is recorded in history (it
//! still cost time) but never in the memo.

use crate::{Result, StoreError, atomic_write, ensure_dir, shard_path};
use flash_core::{ActionKey, Attribution, Digest, NodeKey, Verdict};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MemoEntry {
    pub action: ActionKey,
    /// The logical node that produced this, kept for provenance and debugging.
    pub node_key: NodeKey,
    pub outputs: Vec<Digest>,
    /// Content hash of the subgraph this node computed, when it delegated to one. Cached with
    /// the node so that a hit on a planner rebuilds the plan without calling the planner.
    #[serde(default)]
    pub expansion: Option<Digest>,
    pub attribution: Attribution,
    /// Wall time of the original computation. A hit reports this as "time saved".
    pub duration_ms: u64,
    pub recorded_at_unix_ms: u64,
}

pub struct MemoStore {
    root: PathBuf,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl MemoStore {
    pub fn open(root: PathBuf) -> Result<Self> {
        ensure_dir(&root)?;
        Ok(MemoStore {
            root,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        })
    }

    fn path(&self, action: &ActionKey) -> PathBuf {
        shard_path(&self.root, &action.0.hex(), ".json")
    }

    /// Look up without counting a hit or a miss.
    ///
    /// The eta walk probes the memo for nodes that have not run yet, to find out how much of the
    /// remaining graph is already cached. Those probes are not cache traffic and must not show up
    /// in the counters the benchmark reports.
    pub fn peek(&self, action: &ActionKey) -> Result<Option<MemoEntry>> {
        let path = self.path(action);
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|source| StoreError::Corrupt { path, source }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(StoreError::Io { path, source }),
        }
    }

    pub fn get(&self, action: &ActionKey) -> Result<Option<MemoEntry>> {
        let path = self.path(action);
        match std::fs::read(&path) {
            Ok(bytes) => {
                let entry: MemoEntry = serde_json::from_slice(&bytes)
                    .map_err(|source| StoreError::Corrupt { path, source })?;
                self.hits.fetch_add(1, Ordering::Relaxed);
                Ok(Some(entry))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                Ok(None)
            }
            Err(source) => Err(StoreError::Io { path, source }),
        }
    }

    /// Record a result. Returns false without writing if the verdict did not pass.
    ///
    /// The argument list is long because every one of these is part of what gets cached, and
    /// bundling them into a struct here would only move the same fields somewhere the caller has
    /// to fill in anyway.
    #[allow(clippy::too_many_arguments)]
    pub fn put(
        &self,
        action: ActionKey,
        node_key: &NodeKey,
        verdict: Verdict,
        outputs: &[Digest],
        attribution: Attribution,
        duration_ms: u64,
        expansion: Option<Digest>,
    ) -> Result<bool> {
        if !verdict.passed() {
            return Ok(false);
        }
        let entry = MemoEntry {
            action,
            node_key: node_key.clone(),
            outputs: outputs.to_vec(),
            expansion,
            attribution,
            duration_ms,
            recorded_at_unix_ms: now_ms(),
        };
        let bytes = serde_json::to_vec_pretty(&entry).expect("memo entry is serializable");
        atomic_write(&self.path(&action), &bytes)?;
        Ok(true)
    }

    pub fn counters(&self) -> (u64, u64) {
        (
            self.hits.load(Ordering::Relaxed),
            self.misses.load(Ordering::Relaxed),
        )
    }
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, MemoStore) {
        let dir = tempfile::tempdir().unwrap();
        let ms = MemoStore::open(dir.path().join("memo")).unwrap();
        (dir, ms)
    }

    #[test]
    fn passing_results_round_trip() {
        let (_d, ms) = store();
        let action = ActionKey(Digest::of(b"a"));
        let out = vec![Digest::of(b"out")];
        let wrote = ms
            .put(
                action,
                &NodeKey::new("n"),
                Verdict::Pass,
                &out,
                Attribution::default(),
                42,
                None,
            )
            .unwrap();
        assert!(wrote);
        let got = ms.get(&action).unwrap().expect("hit");
        assert_eq!(got.outputs, out);
        assert_eq!(got.duration_ms, 42);
    }

    #[test]
    fn failures_are_never_memoized() {
        let (_d, ms) = store();
        let action = ActionKey(Digest::of(b"b"));
        let wrote = ms
            .put(
                action,
                &NodeKey::new("n"),
                Verdict::Fail,
                &[],
                Attribution::default(),
                7,
                None,
            )
            .unwrap();
        assert!(!wrote, "a failing node must not be written to the memo");
        assert!(ms.get(&action).unwrap().is_none());
    }
}

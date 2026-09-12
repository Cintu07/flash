//! Duration history: the last N wall times per logical node key.
//!
//! Keyed on [`NodeKey`], not [`flash_core::ActionKey`]. A node run against changed inputs has a
//! brand new action key every time, but its *cost* is a property of the step ("render 30 pages",
//! "typecheck this crate"), not of the bytes it happened to consume. Keying history on the action
//! key would mean every cold node is always "unknown", which is exactly the case where an eta is
//! worth having.
//!
//! Failures are recorded too. They cost real time, and an eta that ignores the retry path lies.

use crate::{Result, StoreError, atomic_replace, ensure_dir, shard_path};
use flash_core::NodeKey;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

/// How many samples to keep per node. Section 3.1 says 20.
pub const RING: usize = 20;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Record {
    node_key: String,
    /// Oldest first. Truncated to RING on write.
    samples_ms: Vec<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stats {
    pub p50_ms: u64,
    pub p90_ms: u64,
    pub samples: usize,
}

pub struct History {
    root: PathBuf,
    /// In-memory mirror. The file is the durable copy; this avoids a read-modify-write per
    /// completion and makes eta computation free.
    cache: Mutex<HashMap<NodeKey, Vec<u64>>>,
}

impl History {
    pub fn open(root: PathBuf) -> Result<Self> {
        ensure_dir(&root)?;
        Ok(History {
            root,
            cache: Mutex::new(HashMap::new()),
        })
    }

    fn path(&self, key: &NodeKey) -> PathBuf {
        shard_path(&self.root, &key.digest().hex(), ".json")
    }

    fn load(&self, key: &NodeKey) -> Result<Vec<u64>> {
        let path = self.path(key);
        match std::fs::read(&path) {
            Ok(bytes) => {
                let rec: Record = serde_json::from_slice(&bytes)
                    .map_err(|source| StoreError::Corrupt { path, source })?;
                Ok(rec.samples_ms)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(source) => Err(StoreError::Io { path, source }),
        }
    }

    /// Append one wall time and persist.
    pub fn record(&self, key: &NodeKey, duration_ms: u64) -> Result<()> {
        let mut samples = {
            let cache = self.cache.lock().unwrap();
            match cache.get(key) {
                Some(v) => v.clone(),
                None => self.load(key)?,
            }
        };
        samples.push(duration_ms);
        if samples.len() > RING {
            let drop = samples.len() - RING;
            samples.drain(0..drop);
        }
        let rec = Record {
            node_key: key.0.clone(),
            samples_ms: samples.clone(),
        };
        let bytes = serde_json::to_vec(&rec).expect("history record is serializable");
        atomic_replace(&self.path(key), &bytes)?;
        self.cache.lock().unwrap().insert(key.clone(), samples);
        Ok(())
    }

    /// p50 and p90 for a node, or None when there is no history.
    ///
    /// None is reported as "unknown" rather than guessed (section 3.1). A made up eta is worse
    /// than no eta: it trains the user to ignore the number.
    pub fn stats(&self, key: &NodeKey) -> Option<Stats> {
        let mut cache = self.cache.lock().unwrap();
        let samples = match cache.get(key) {
            Some(v) => v.clone(),
            None => {
                let v = self.load(key).unwrap_or_default();
                cache.insert(key.clone(), v.clone());
                v
            }
        };
        if samples.is_empty() {
            return None;
        }
        let mut sorted = samples.clone();
        sorted.sort_unstable();
        Some(Stats {
            p50_ms: percentile(&sorted, 50),
            p90_ms: percentile(&sorted, 90),
            samples: sorted.len(),
        })
    }
}

/// Nearest-rank percentile. With at most 20 samples, interpolation would be false precision.
fn percentile(sorted: &[u64], p: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (p * sorted.len()).div_ceil(100);
    let idx = rank.saturating_sub(1).min(sorted.len() - 1);
    sorted[idx]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_until_there_is_history() {
        let dir = tempfile::tempdir().unwrap();
        let h = History::open(dir.path().join("h")).unwrap();
        assert!(h.stats(&NodeKey::new("fresh")).is_none());
    }

    #[test]
    fn percentiles_track_the_samples() {
        let dir = tempfile::tempdir().unwrap();
        let h = History::open(dir.path().join("h")).unwrap();
        let k = NodeKey::new("n");
        for ms in [10u64, 20, 30, 40, 100] {
            h.record(&k, ms).unwrap();
        }
        let s = h.stats(&k).unwrap();
        assert_eq!(s.samples, 5);
        assert_eq!(s.p50_ms, 30);
        assert_eq!(s.p90_ms, 100);
    }

    #[test]
    fn ring_keeps_only_the_last_twenty() {
        let dir = tempfile::tempdir().unwrap();
        let h = History::open(dir.path().join("h")).unwrap();
        let k = NodeKey::new("n");
        for i in 0..50u64 {
            h.record(&k, i).unwrap();
        }
        let s = h.stats(&k).unwrap();
        assert_eq!(s.samples, RING);
        // Samples 30..=49 survive, so the median is 39 or 40, never something from the first half.
        assert!(s.p50_ms >= 39, "stale samples leaked into the ring: {s:?}");
    }

    #[test]
    fn history_survives_reopening_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let k = NodeKey::new("n");
        {
            let h = History::open(dir.path().join("h")).unwrap();
            h.record(&k, 77).unwrap();
        }
        let h2 = History::open(dir.path().join("h")).unwrap();
        assert_eq!(h2.stats(&k).unwrap().p50_ms, 77);
    }
}

//! Run events and the run report.
//!
//! The report is not a log. It is the raw material for d11: per step attribution, time to first
//! visible change, and the cold / warm / hot split. Anything the benchmark needs to publish has
//! to be recoverable from here without re-running anything.

use crate::eta::Eta;
use crate::exec::{JobId, Progress};
use flash_core::{ActionKey, Attribution, Digest, NodeKey, NodeKind};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use tokio::sync::mpsc::UnboundedSender;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeStatus {
    /// Served from the memo store. Zero work.
    Hit,
    /// Found already complete in this task's journal after a crash or disconnect.
    Resumed,
    /// Actually executed.
    Computed,
    /// Executed and its verdict was Fail.
    Failed,
    /// Never ran: something upstream failed.
    Skipped,
    /// Ran, and handed its result to a subgraph it computed. Its own report carries that
    /// subgraph's outputs once the substitute finishes.
    Expanded,
}

impl NodeStatus {
    pub fn did_work(&self) -> bool {
        matches!(
            self,
            NodeStatus::Computed | NodeStatus::Failed | NodeStatus::Expanded
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeReport {
    pub key: NodeKey,
    pub kind: NodeKind,
    pub action: Option<ActionKey>,
    pub status: NodeStatus,
    pub outputs: Vec<Digest>,
    /// Execution time, excluding time queued behind a lane cap.
    pub exec_ms: u64,
    /// Time spent waiting for a slot. Contention, not work.
    pub queue_ms: u64,
    pub attribution: Attribution,
    pub was_job: bool,
    /// Milliseconds after task start when this node finished.
    pub finished_at_ms: u64,
    pub diagnostics: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunReport {
    pub task_id: String,
    pub wall_ms: u64,
    /// Eta computed before any node ran, from history alone.
    pub eta_at_start: Eta,
    /// When the first node produced output. The "time to first visible change" metric (d11).
    pub first_change_ms: Option<u64>,
    pub nodes: Vec<NodeReport>,
    /// Summed attribution across every node that did work.
    pub attribution: Attribution,
    /// Wall time the memo store saved, as originally measured on the nodes that hit.
    pub saved_ms: u64,
}

impl RunReport {
    pub fn count(&self, status: NodeStatus) -> usize {
        self.nodes.iter().filter(|n| n.status == status).count()
    }

    pub fn hits(&self) -> usize {
        self.count(NodeStatus::Hit) + self.count(NodeStatus::Resumed)
    }

    pub fn computed(&self) -> usize {
        self.nodes.iter().filter(|n| n.status.did_work()).count()
    }

    /// Nodes that expanded into a subgraph rather than producing a result directly.
    pub fn expanded(&self) -> usize {
        self.count(NodeStatus::Expanded)
    }

    pub fn failed(&self) -> usize {
        self.count(NodeStatus::Failed)
    }

    pub fn ok(&self) -> bool {
        self.failed() == 0 && self.count(NodeStatus::Skipped) == 0
    }

    /// Exactly which nodes did work. The dirty closure test asserts on this set.
    pub fn computed_keys(&self) -> BTreeSet<NodeKey> {
        self.nodes
            .iter()
            .filter(|n| n.status.did_work())
            .map(|n| n.key.clone())
            .collect()
    }

    pub fn node(&self, key: &str) -> Option<&NodeReport> {
        self.nodes.iter().find(|n| n.key.as_str() == key)
    }

    /// A hot run is one where nothing was computed at all.
    pub fn is_hot(&self) -> bool {
        self.computed() == 0 && !self.nodes.is_empty()
    }
}

/// What the runtime tells a client while a task runs.
#[derive(Clone, Debug)]
pub enum Event {
    GraphPlanned {
        task_id: String,
        nodes: usize,
    },
    EtaUpdated {
        eta: Eta,
    },
    NodeHit {
        key: NodeKey,
        action: ActionKey,
        saved_ms: u64,
    },
    NodeStarted {
        key: NodeKey,
        action: ActionKey,
        job: Option<JobId>,
    },
    JobProgress {
        key: NodeKey,
        job: JobId,
        progress: Progress,
    },
    NodeFinished {
        key: NodeKey,
        status: NodeStatus,
        exec_ms: u64,
    },
    NodeSkipped {
        key: NodeKey,
        because: NodeKey,
    },
    /// A node computed a subgraph and handed its identity to one node in it.
    GraphExpanded {
        key: NodeKey,
        added: usize,
        substitute: NodeKey,
    },
    TaskFinished {
        task_id: String,
        wall_ms: u64,
    },
}

/// Where events go. Cloneable and cheap; a client that hangs up just stops receiving.
#[derive(Clone, Default)]
pub struct Events(Option<UnboundedSender<Event>>);

impl Events {
    /// Drop everything. Used by tests and by headless runs.
    pub fn none() -> Self {
        Events(None)
    }

    pub fn channel() -> (Events, tokio::sync::mpsc::UnboundedReceiver<Event>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (Events(Some(tx)), rx)
    }

    pub fn emit(&self, e: Event) {
        if let Some(tx) = &self.0 {
            let _ = tx.send(e);
        }
    }
}

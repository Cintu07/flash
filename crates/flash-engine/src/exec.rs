//! What it takes to execute one node, and the job machinery around slow ones.
//!
//! d5: a node whose p50 is over the threshold, or which is a render or build by nature, runs as a
//! *job*: it gets an id, a progress stream and a cancel handle, and the scheduler never blocks on
//! it. Everything here is async, so "never blocks" is structural rather than a promise: the
//! dispatch loop hands a job to the runtime and goes straight back to dispatching ready nodes.

use crate::graph::NodeSpec;
use flash_core::{ActionKey, Digest, NodeKey, NodeOutput};
use flash_store::Store;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, watch};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct JobId(pub u64);

impl std::fmt::Display for JobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "job-{}", self.0)
    }
}

/// A progress report from inside a running job.
#[derive(Clone, Debug, PartialEq)]
pub struct Progress {
    /// 0.0 to 1.0 when the job can measure itself, None when it can only name a phase.
    pub fraction: Option<f32>,
    pub phase: String,
}

impl Progress {
    pub fn phase(p: impl Into<String>) -> Self {
        Progress {
            fraction: None,
            phase: p.into(),
        }
    }

    pub fn at(fraction: f32, phase: impl Into<String>) -> Self {
        Progress {
            fraction: Some(fraction.clamp(0.0, 1.0)),
            phase: phase.into(),
        }
    }
}

/// The job side of an execution: report progress, notice cancellation.
#[derive(Clone)]
pub struct JobHandle {
    pub id: JobId,
    progress: mpsc::UnboundedSender<Progress>,
    cancel: watch::Receiver<bool>,
}

impl JobHandle {
    pub fn report(&self, p: Progress) {
        let _ = self.progress.send(p);
    }

    /// Long jobs must poll this. A cancelled job should stop promptly and return a Fail verdict,
    /// which by the memoization rule means nothing is written to the cache.
    pub fn cancelled(&self) -> bool {
        *self.cancel.borrow()
    }

    /// Wait until cancelled. Useful in a `select!` against the real work.
    pub async fn cancelled_owned(&self) {
        let mut rx = self.cancel.clone();
        loop {
            if *rx.borrow() {
                return;
            }
            if rx.changed().await.is_err() {
                return;
            }
        }
    }
}

struct JobEntry {
    node_key: NodeKey,
    cancel: watch::Sender<bool>,
}

/// Live jobs for a daemon. Survives client disconnect by construction: the registry belongs to
/// the engine, not to whoever asked for the task (section 3.2).
#[derive(Default)]
pub struct JobRegistry {
    next: Mutex<u64>,
    live: Mutex<HashMap<JobId, JobEntry>>,
}

impl JobRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(JobRegistry::default())
    }

    pub(crate) fn register(
        &self,
        node_key: &NodeKey,
    ) -> (JobHandle, mpsc::UnboundedReceiver<Progress>) {
        let id = {
            let mut n = self.next.lock().unwrap();
            *n += 1;
            JobId(*n)
        };
        let (tx, rx) = mpsc::unbounded_channel();
        let (cancel_tx, cancel_rx) = watch::channel(false);
        self.live.lock().unwrap().insert(
            id,
            JobEntry {
                node_key: node_key.clone(),
                cancel: cancel_tx,
            },
        );
        (
            JobHandle {
                id,
                progress: tx,
                cancel: cancel_rx,
            },
            rx,
        )
    }

    pub(crate) fn finish(&self, id: JobId) {
        self.live.lock().unwrap().remove(&id);
    }

    /// Ask a job to stop. Returns false if it already finished.
    pub fn cancel(&self, id: JobId) -> bool {
        match self.live.lock().unwrap().get(&id) {
            Some(e) => e.cancel.send(true).is_ok(),
            None => false,
        }
    }

    pub fn cancel_all(&self) {
        for e in self.live.lock().unwrap().values() {
            let _ = e.cancel.send(true);
        }
    }

    pub fn live(&self) -> Vec<(JobId, NodeKey)> {
        let mut v: Vec<_> = self
            .live
            .lock()
            .unwrap()
            .iter()
            .map(|(id, e)| (*id, e.node_key.clone()))
            .collect();
        v.sort();
        v
    }
}

/// Everything a node needs to run.
pub struct ExecCtx {
    pub spec: Arc<NodeSpec>,
    /// Content hashes this node's dependencies produced, in declared dependency order.
    pub inputs: Vec<Digest>,
    /// The memo key this execution will be filed under, should it pass.
    pub action: ActionKey,
    pub store: Arc<Store>,
    /// Present only when this node was classified as a job.
    pub job: Option<JobHandle>,
}

impl ExecCtx {
    pub fn key(&self) -> &NodeKey {
        &self.spec.key
    }

    pub fn report(&self, p: Progress) {
        if let Some(j) = &self.job {
            j.report(p);
        }
    }

    pub fn cancelled(&self) -> bool {
        self.job.as_ref().is_some_and(|j| j.cancelled())
    }
}

/// A node replaced by the subgraph it just computed.
///
/// This is the dynamic half of the build graph, and it is what the PRD's planner needs: a plan
/// node cannot declare its children in advance, because computing them *is* its job. The same
/// primitive covers repair: a verify rung that fails emits a fix-and-recheck subgraph and hands
/// its own identity to the recheck.
///
/// The engine treats the expanding node as delegating: its outputs become the substitute's
/// outputs, and its dependents wait for the substitute rather than for it. Because the
/// substitute's action key is computed from real content (diagnostics included), a repair that
/// has been done before is a memo hit like anything else.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Expansion {
    /// New nodes. They may depend on each other and on nodes already in the graph, but never on
    /// a dependent of the expanding node: that would be a cycle.
    pub nodes: Vec<NodeSpec>,
    /// Which of the new nodes stands in for the expanding one.
    pub substitute: NodeKey,
}

impl Expansion {
    pub fn new(nodes: Vec<NodeSpec>, substitute: impl Into<String>) -> Self {
        Expansion {
            nodes,
            substitute: NodeKey::new(substitute),
        }
    }
}

/// What an executor hands back: an output, and optionally a subgraph that supersedes it.
#[derive(Debug)]
pub struct NodeResult {
    pub output: NodeOutput,
    pub expansion: Option<Expansion>,
}

impl NodeResult {
    pub fn done(output: NodeOutput) -> Self {
        NodeResult {
            output,
            expansion: None,
        }
    }

    /// Delegate to a subgraph. The node itself is recorded as passing: it did its job, which was
    /// to work out what the real work is.
    pub fn expand(outputs: Vec<Digest>, expansion: Expansion) -> Self {
        NodeResult {
            output: NodeOutput::pass(outputs),
            expansion: Some(expansion),
        }
    }
}

impl From<NodeOutput> for NodeResult {
    fn from(output: NodeOutput) -> Self {
        NodeResult::done(output)
    }
}

pub type ExecFuture = Pin<Box<dyn Future<Output = NodeResult> + Send>>;

/// How the engine runs a node. Adapters and model clients implement this.
///
/// Returning a `Fail` verdict is normal control flow (a ladder rung caught something); it is not
/// an error. Engine errors are reserved for the store and the graph.
pub trait NodeExecutor: Send + Sync + 'static {
    fn execute(&self, ctx: ExecCtx) -> ExecFuture;
}

impl<F> NodeExecutor for F
where
    F: Fn(ExecCtx) -> ExecFuture + Send + Sync + 'static,
{
    fn execute(&self, ctx: ExecCtx) -> ExecFuture {
        (self)(ctx)
    }
}

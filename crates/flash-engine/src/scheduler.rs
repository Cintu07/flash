//! The scheduler: resolve action keys, serve what is cached, run what is not.
//!
//! The loop is small on purpose:
//!
//! 1. Take every node whose dependencies are resolved.
//! 2. Compute its action key *from the content its inputs actually produced*.
//! 3. Memo hit, so return the outputs and do no work. This cascades: a hit immediately unlocks
//!    its children, which are usually hits too, which is why a hot run is sub second rather than
//!    "fast".
//! 4. Otherwise dispatch it. Slow nodes become jobs; the loop never waits on one.
//!
//! Early cutoff falls out of step 2. A node that reruns and produces byte-identical output leaves
//! every downstream action key unchanged, so the downstream still hits. This is why the dirty
//! closure that actually runs is usually smaller than the structural closure of the change.
//!
//! ## The graph is not fixed
//!
//! A build system gets to demand the whole graph up front. An agent runtime cannot: the planner's
//! job *is* to produce the graph, and a verify rung that fails produces repair work nobody
//! planned. So a node may return an [`Expansion`]: a subgraph, plus the one node in it that
//! stands in for the expanding node. Dependents then wait on the substitute, whose action key is
//! computed from real content like everything else, so repairs and plans are cached exactly like
//! ordinary work. The expansion is stored alongside the memo entry, so a *hit* on a planner node
//! rebuilds the subgraph it planned without calling the planner at all.

use crate::eta::{self, NodeCost};
use crate::exec::{ExecCtx, Expansion, JobHandle, JobId, JobRegistry, NodeExecutor};
use crate::graph::{GraphError, NodeSpec, TaskGraph};
use crate::report::{Event, Events, NodeReport, NodeStatus, RunReport};
use flash_core::{ActionKey, Attribution, Digest, Lane, NodeKey, NodeOutput, Verdict, action_key};
use flash_store::{JournalEntry, Store, StoreError};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Graph(#[from] GraphError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("a node task panicked: {0}")]
    Panicked(String),
    #[error("node {node} expanded into a subgraph that does not contain its substitute {sub}")]
    BadExpansion { node: NodeKey, sub: NodeKey },
    #[error("node {0} expanded past the depth limit: a repair loop is not terminating")]
    ExpansionDepth(NodeKey),
}

#[derive(Clone, Debug)]
pub struct EngineConfig {
    /// Per lane concurrency cap. Cpu-heavy lanes get fewer slots (section 3.1).
    pub lane_caps: HashMap<Lane, usize>,
    pub default_lane_cap: usize,
    /// A node whose p50 exceeds this becomes a job (d5: 2 seconds).
    pub job_threshold_ms: u64,
    /// Resume a previously crashed or disconnected task from its journal.
    pub resume: bool,
    /// How deep one lineage of expansions may go. A repair that keeps producing repairs is a
    /// bug, and an unbounded one burns tokens until a human notices.
    pub max_expansion_depth: usize,
}

impl Default for EngineConfig {
    fn default() -> Self {
        let cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        let mut lane_caps = HashMap::new();
        // Model calls are network bound: many in flight is free.
        lane_caps.insert(Lane::model(), 32);
        // Local work is not.
        lane_caps.insert(Lane::cpu(), cpus.max(2));
        EngineConfig {
            lane_caps,
            default_lane_cap: cpus.max(2),
            job_threshold_ms: 2_000,
            resume: true,
            max_expansion_depth: 8,
        }
    }
}

pub struct Engine {
    store: Arc<Store>,
    cfg: EngineConfig,
    jobs: Arc<JobRegistry>,
    lanes: Mutex<HashMap<Lane, Arc<Semaphore>>>,
}

struct Completion {
    spec: Arc<NodeSpec>,
    action: ActionKey,
    output: NodeOutput,
    expansion: Option<Expansion>,
    exec_ms: u64,
    queue_ms: u64,
    job: Option<JobId>,
}

impl Engine {
    pub fn new(store: Arc<Store>, cfg: EngineConfig) -> Arc<Self> {
        Arc::new(Engine {
            store,
            cfg,
            jobs: JobRegistry::new(),
            lanes: Mutex::new(HashMap::new()),
        })
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    pub fn config(&self) -> &EngineConfig {
        &self.cfg
    }

    /// Live jobs, for a client that wants to show or cancel them.
    pub fn jobs(&self) -> &Arc<JobRegistry> {
        &self.jobs
    }

    fn lane(&self, lane: &Lane) -> Arc<Semaphore> {
        let mut lanes = self.lanes.lock().unwrap();
        lanes
            .entry(lane.clone())
            .or_insert_with(|| {
                let cap = self
                    .cfg
                    .lane_caps
                    .get(lane)
                    .copied()
                    .unwrap_or(self.cfg.default_lane_cap)
                    .max(1);
                Arc::new(Semaphore::new(cap))
            })
            .clone()
    }

    /// Should this node run as a job? d5: over the threshold by history, or slow by nature.
    fn is_job(&self, spec: &NodeSpec) -> bool {
        if spec.kind.is_inherently_slow() {
            return true;
        }
        self.store
            .history
            .stats(&spec.key)
            .is_some_and(|s| s.p50_ms > self.cfg.job_threshold_ms)
    }

    /// What every remaining node is expected to cost, including the ones that will never run.
    ///
    /// The naive version asks history what each unfinished node costs and adds up the critical
    /// path. On a warm or hot task that is badly wrong: it quotes the user 30 seconds for work
    /// the memo store is about to serve in 4 milliseconds. So this walks the topo order
    /// *predicting* content forward - a node whose inputs are all predicted gets its action key
    /// computed and probed against the memo, and a probe that hits costs nothing and lets the
    /// prediction continue through its children. The moment a node cannot be predicted, its
    /// descendants fall back to history, which is the honest answer.
    ///
    /// This is what makes "eta is real from the first second" true rather than aspirational.
    fn costs(&self, st: &RunState) -> HashMap<NodeKey, NodeCost> {
        let mut costs = HashMap::with_capacity(st.topo.len());
        let mut predicted: HashMap<NodeKey, Vec<Digest>> = HashMap::new();

        for key in &st.topo {
            let Some(spec) = st.graph.get(key) else {
                continue;
            };

            if st.settled.contains(key) {
                costs.insert(key.clone(), NodeCost::Done);
                if let Some(outs) = st.outputs.get(key) {
                    predicted.insert(key.clone(), outs.clone());
                }
                continue;
            }

            // Predict this node's inputs from what upstream is expected to produce.
            let mut inputs = Vec::new();
            let mut predictable = true;
            for d in &spec.deps {
                match predicted.get(d) {
                    Some(outs) => inputs.extend_from_slice(outs),
                    None => {
                        predictable = false;
                        break;
                    }
                }
            }

            if predictable && !st.running.contains_key(key) {
                let action = action_key(spec.kind, &spec.op, &spec.env, &inputs);
                if let Ok(Some(entry)) = self.store.memo.peek(&action) {
                    costs.insert(key.clone(), NodeCost::Done);
                    predicted.insert(key.clone(), entry.outputs);
                    continue;
                }
            }

            let cost = match self.store.history.stats(key) {
                Some(s) => {
                    let elapsed = st
                        .running
                        .get(key)
                        .map(|t| t.elapsed().as_millis() as u64)
                        .unwrap_or(0);
                    NodeCost::Known(s.p50_ms.saturating_sub(elapsed))
                }
                None => NodeCost::Unknown,
            };
            costs.insert(key.clone(), cost);
        }
        costs
    }

    pub async fn run(
        &self,
        task_id: &str,
        graph: &TaskGraph,
        executor: Arc<dyn NodeExecutor>,
        events: Events,
    ) -> Result<RunReport, EngineError> {
        let plan = graph.validate()?;
        let t0 = Instant::now();
        events.emit(Event::GraphPlanned {
            task_id: task_id.to_string(),
            nodes: graph.len(),
        });

        let resume: HashMap<NodeKey, JournalEntry> = if self.cfg.resume {
            self.store
                .journal
                .replay(task_id)?
                .into_iter()
                .map(|e| (e.node_key.clone(), e))
                .collect()
        } else {
            HashMap::new()
        };

        let mut st = RunState {
            graph: graph.clone(),
            topo: plan.topo.clone(),
            dependents: plan.dependents.clone(),
            pending: plan.indegree.clone(),
            outputs: HashMap::new(),
            ready: plan
                .topo
                .iter()
                .filter(|k| plan.indegree.get(*k).copied().unwrap_or(0) == 0)
                .cloned()
                .collect(),
            reports: BTreeMap::new(),
            settled: HashSet::new(),
            running: HashMap::new(),
            delegated: HashMap::new(),
            depth: HashMap::new(),
            first_change_ms: None,
            saved_ms: 0,
            attribution: Attribution::default(),
            max_depth: self.cfg.max_expansion_depth,
            t0,
        };

        let eta_at_start = eta::estimate_over(&st.topo, &st.graph, &self.costs(&st));
        events.emit(Event::EtaUpdated { eta: eta_at_start });

        let mut join: JoinSet<Completion> = JoinSet::new();

        loop {
            // Dispatch everything that is ready. Hits are settled inline and can unlock more
            // ready nodes, so this drains rather than iterating a snapshot.
            while let Some(key) = st.ready.pop_front() {
                if st.settled.contains(&key) || st.running.contains_key(&key) {
                    continue;
                }
                let spec = match st.graph.get(&key) {
                    Some(s) => s.clone(),
                    None => continue,
                };
                let inputs = st.inputs_for(&spec);
                let action = action_key(spec.kind, &spec.op, &spec.env, &inputs);

                // Resume: this task already finished this exact action before the crash.
                if let Some(entry) = resume.get(&key)
                    && entry.action == action
                {
                    let outputs = entry.outputs.clone();
                    st.settle(
                        &spec,
                        Some(action),
                        NodeStatus::Resumed,
                        outputs,
                        0,
                        0,
                        entry.attribution,
                        false,
                        Vec::new(),
                        &events,
                    );
                    continue;
                }

                // Memo hit: zero work. A planner node's expansion was cached with it, so the
                // subgraph it planned comes back without the planner running.
                if let Some(entry) = self.store.memo.get(&action)? {
                    let replay = match entry.expansion {
                        Some(d) => {
                            let bytes = self.store.content.get(&d)?;
                            serde_json::from_slice::<Expansion>(&bytes).ok()
                        }
                        None => None,
                    };
                    st.saved_ms += entry.duration_ms;
                    events.emit(Event::NodeHit {
                        key: key.clone(),
                        action,
                        saved_ms: entry.duration_ms,
                    });
                    self.store.journal.append(
                        task_id,
                        &JournalEntry {
                            node_key: key.clone(),
                            action,
                            outputs: entry.outputs.clone(),
                            hit: true,
                            duration_ms: 0,
                            attribution: Attribution::default(),
                            at_unix_ms: now_ms(),
                        },
                    )?;

                    match replay {
                        Some(exp) => {
                            st.apply_expansion(&spec, exp, &events)?;
                            st.record_delegating(&spec, Some(action), NodeStatus::Hit, &events);
                        }
                        None => st.settle(
                            &spec,
                            Some(action),
                            NodeStatus::Hit,
                            entry.outputs,
                            0,
                            0,
                            // A hit costs nothing, so it contributes nothing to attribution. The
                            // work it avoided is reported separately as saved_ms.
                            Attribution::default(),
                            false,
                            Vec::new(),
                            &events,
                        ),
                    }
                    continue;
                }

                // Miss: run it.
                let was_job = self.is_job(&spec);
                let job = if was_job {
                    let (handle, mut rx) = self.jobs.register(&spec.key);
                    let ev = events.clone();
                    let k = spec.key.clone();
                    let id = handle.id;
                    tokio::spawn(async move {
                        while let Some(p) = rx.recv().await {
                            ev.emit(Event::JobProgress {
                                key: k.clone(),
                                job: id,
                                progress: p,
                            });
                        }
                    });
                    Some(handle)
                } else {
                    None
                };

                events.emit(Event::NodeStarted {
                    key: key.clone(),
                    action,
                    job: job.as_ref().map(|j| j.id),
                });
                st.running.insert(key.clone(), Instant::now());

                let sem = self.lane(&spec.lane);
                let store = self.store.clone();
                let exec = executor.clone();
                let spec_for_task = spec.clone();
                let job_for_task: Option<JobHandle> = job;
                join.spawn(async move {
                    let queued_at = Instant::now();
                    let permit = sem
                        .acquire_owned()
                        .await
                        .expect("lane semaphores are never closed");
                    let queue_ms = queued_at.elapsed().as_millis() as u64;
                    let started = Instant::now();
                    let result = exec
                        .execute(ExecCtx {
                            spec: spec_for_task.clone(),
                            inputs,
                            action,
                            store,
                            job: job_for_task.clone(),
                        })
                        .await;
                    let exec_ms = started.elapsed().as_millis() as u64;
                    drop(permit);
                    Completion {
                        spec: spec_for_task,
                        action,
                        output: result.output,
                        expansion: result.expansion,
                        exec_ms,
                        queue_ms,
                        job: job_for_task.map(|j| j.id),
                    }
                });
            }

            if join.is_empty() {
                break;
            }

            let completion = match join.join_next().await {
                Some(Ok(c)) => c,
                Some(Err(e)) => return Err(EngineError::Panicked(e.to_string())),
                None => break,
            };

            let Completion {
                spec,
                action,
                output,
                expansion,
                exec_ms,
                queue_ms,
                job,
            } = completion;
            st.running.remove(&spec.key);
            if let Some(id) = job {
                self.jobs.finish(id);
            }

            // History records what the step cost, pass or fail. A retry path that is never
            // measured makes every eta on that node optimistic.
            self.store.history.record(&spec.key, exec_ms)?;

            let passed = output.verdict.passed();
            if passed {
                // The expansion is part of the result, so it is cached with it. That is what
                // lets a hit on a planner node rebuild the plan for free.
                let expansion_digest = match &expansion {
                    Some(exp) => {
                        let bytes = serde_json::to_vec(exp).expect("an expansion is serializable");
                        Some(self.store.content.put(&bytes)?)
                    }
                    None => None,
                };
                self.store.memo.put(
                    action,
                    &spec.key,
                    Verdict::Pass,
                    &output.outputs,
                    output.attribution,
                    exec_ms,
                    expansion_digest,
                )?;
                self.store.journal.append(
                    task_id,
                    &JournalEntry {
                        node_key: spec.key.clone(),
                        action,
                        outputs: output.outputs.clone(),
                        hit: false,
                        duration_ms: exec_ms,
                        attribution: output.attribution,
                        at_unix_ms: now_ms(),
                    },
                )?;
            }

            match expansion {
                Some(exp) if passed => {
                    st.apply_expansion(&spec, exp, &events)?;
                    st.record_delegating(&spec, Some(action), NodeStatus::Expanded, &events);
                    if let Some(r) = st.reports.get_mut(&spec.key) {
                        r.exec_ms = exec_ms;
                        r.queue_ms = queue_ms;
                        r.was_job = job.is_some();
                        r.attribution = output.attribution;
                    }
                    st.attribution.merge(&output.attribution);
                }
                _ => st.settle(
                    &spec,
                    Some(action),
                    if passed {
                        NodeStatus::Computed
                    } else {
                        NodeStatus::Failed
                    },
                    output.outputs,
                    exec_ms,
                    queue_ms,
                    output.attribution,
                    job.is_some(),
                    output.diagnostics,
                    &events,
                ),
            }

            events.emit(Event::EtaUpdated {
                eta: eta::estimate_over(&st.topo, &st.graph, &self.costs(&st)),
            });
        }

        // Anything still pending never became reachable: it sits behind a failure.
        let leftovers: Vec<NodeKey> = st
            .topo
            .iter()
            .filter(|k| !st.settled.contains(*k))
            .cloned()
            .collect();
        for key in leftovers {
            if let Some(spec) = st.graph.get(&key).cloned() {
                st.settle(
                    &spec,
                    None,
                    NodeStatus::Skipped,
                    Vec::new(),
                    0,
                    0,
                    Attribution::default(),
                    false,
                    Vec::new(),
                    &events,
                );
            }
        }

        // The journal exists to resume an *interrupted* task. Reaching this line means the run
        // got as far as it was going to, so the frontier is no longer meaningful: clearing it
        // means the next invocation of this task is served by the memo store, and reports honest
        // hit / computed counts instead of replaying itself as "resumed". A crash never reaches
        // here, which is exactly when the journal is worth keeping.
        self.store.journal.clear(task_id)?;

        let wall_ms = t0.elapsed().as_millis() as u64;
        events.emit(Event::TaskFinished {
            task_id: task_id.to_string(),
            wall_ms,
        });

        let nodes = st
            .topo
            .iter()
            .filter_map(|k| st.reports.get(k).cloned())
            .collect();
        Ok(RunReport {
            task_id: task_id.to_string(),
            wall_ms,
            eta_at_start,
            first_change_ms: st.first_change_ms,
            nodes,
            attribution: st.attribution,
            saved_ms: st.saved_ms,
        })
    }
}

struct RunState {
    /// The live graph. It grows when a node expands.
    graph: TaskGraph,
    topo: Vec<NodeKey>,
    dependents: HashMap<NodeKey, Vec<NodeKey>>,
    pending: HashMap<NodeKey, usize>,
    outputs: HashMap<NodeKey, Vec<Digest>>,
    ready: VecDeque<NodeKey>,
    reports: BTreeMap<NodeKey, NodeReport>,
    /// Every node that will not run again this task: done, failed, skipped or delegated.
    settled: HashSet<NodeKey>,
    running: HashMap<NodeKey, Instant>,
    /// substitute -> the node whose identity it took over.
    delegated: HashMap<NodeKey, NodeKey>,
    /// How many expansions deep each node's lineage is.
    depth: HashMap<NodeKey, usize>,
    first_change_ms: Option<u64>,
    saved_ms: u64,
    attribution: Attribution,
    max_depth: usize,
    t0: Instant,
}

impl RunState {
    /// Input digests in declared dependency order. A dependency contributing several outputs
    /// contributes them all, in its own order.
    fn inputs_for(&self, spec: &NodeSpec) -> Vec<Digest> {
        let mut inputs = Vec::new();
        for d in &spec.deps {
            if let Some(outs) = self.outputs.get(d) {
                inputs.extend_from_slice(outs);
            }
        }
        inputs
    }

    /// Splice a computed subgraph into the running graph.
    ///
    /// The names in an expansion are *relative to the node that emitted it*: a node called
    /// "recheck" emitted by "verify:lsp" is spliced in as "verify:lsp/recheck". Two reasons, both
    /// found by testing. Absolute names collide the moment two planners emit a section with the
    /// same title. Worse, an expansion is cached against an *action* key, and two different
    /// logical nodes with identical inputs share one action key by design, so a cached expansion
    /// can legitimately be replayed under a different parent; with absolute names that replay
    /// tries to add nodes that already exist.
    fn apply_expansion(
        &mut self,
        parent: &Arc<NodeSpec>,
        exp: Expansion,
        events: &Events,
    ) -> Result<(), EngineError> {
        let siblings: HashSet<String> = exp.nodes.iter().map(|n| n.key.0.clone()).collect();
        if !siblings.contains(exp.substitute.as_str()) {
            return Err(EngineError::BadExpansion {
                node: parent.key.clone(),
                sub: exp.substitute.clone(),
            });
        }

        let depth = self.depth.get(&parent.key).copied().unwrap_or(0) + 1;
        if depth > self.max_depth {
            return Err(EngineError::ExpansionDepth(parent.key.clone()));
        }

        let prefix = format!("{}/", parent.key.as_str());
        let qualify = |k: &NodeKey| -> NodeKey {
            if siblings.contains(k.as_str()) {
                NodeKey::new(format!("{prefix}{}", k.as_str()))
            } else {
                // Not a sibling, so it names a node that already exists in the graph.
                k.clone()
            }
        };

        let nodes: Vec<NodeSpec> = exp
            .nodes
            .into_iter()
            .map(|mut n| {
                n.deps = n.deps.iter().map(&qualify).collect();
                n.key = NodeKey::new(format!("{prefix}{}", n.key.as_str()));
                n
            })
            .collect();
        let substitute = qualify(&exp.substitute);

        let added = self.graph.add_batch(nodes)?;

        for key in &added {
            self.depth.insert(key.clone(), depth);
            let spec = self.graph.get(key).expect("just added").clone();
            let mut outstanding = 0usize;
            for d in &spec.deps {
                if !self.settled.contains(d) {
                    outstanding += 1;
                }
                self.dependents
                    .entry(d.clone())
                    .or_default()
                    .push(key.clone());
            }
            self.pending.insert(key.clone(), outstanding);
            self.topo.push(key.clone());
            if outstanding == 0 {
                self.ready.push_back(key.clone());
            }
        }

        self.delegated
            .insert(substitute.clone(), parent.key.clone());
        events.emit(Event::GraphExpanded {
            key: parent.key.clone(),
            added: added.len(),
            substitute,
        });
        Ok(())
    }

    /// Record a node that handed its identity to a substitute. It is settled for scheduling
    /// purposes, but it does not unlock its dependents: they wait for the substitute.
    ///
    /// `status` is how the *expansion itself* was obtained: `Expanded` when the node ran to
    /// produce it, `Hit` when it came back from the memo. Keeping those apart is what lets a
    /// second run of a task that once needed a repair still report as hot.
    fn record_delegating(
        &mut self,
        spec: &Arc<NodeSpec>,
        action: Option<ActionKey>,
        status: NodeStatus,
        events: &Events,
    ) {
        let key = spec.key.clone();
        self.settled.insert(key.clone());
        self.reports.insert(
            key.clone(),
            NodeReport {
                key: key.clone(),
                kind: spec.kind,
                action,
                status,
                outputs: Vec::new(),
                exec_ms: 0,
                queue_ms: 0,
                attribution: Attribution::default(),
                was_job: false,
                finished_at_ms: self.t0.elapsed().as_millis() as u64,
                diagnostics: Vec::new(),
            },
        );
        events.emit(Event::NodeFinished {
            key,
            status,
            exec_ms: 0,
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn settle(
        &mut self,
        spec: &Arc<NodeSpec>,
        action: Option<ActionKey>,
        status: NodeStatus,
        outputs: Vec<Digest>,
        exec_ms: u64,
        queue_ms: u64,
        attribution: Attribution,
        was_job: bool,
        diagnostics: Vec<String>,
        events: &Events,
    ) {
        let key = spec.key.clone();
        if !self.settled.insert(key.clone()) {
            return;
        }
        if !outputs.is_empty() && self.first_change_ms.is_none() {
            self.first_change_ms = Some(self.t0.elapsed().as_millis() as u64);
        }
        if status.did_work() {
            self.attribution.merge(&attribution);
        }
        self.outputs.insert(key.clone(), outputs.clone());
        self.reports.insert(
            key.clone(),
            NodeReport {
                key: key.clone(),
                kind: spec.kind,
                action,
                status,
                outputs: outputs.clone(),
                exec_ms,
                queue_ms,
                attribution,
                was_job,
                finished_at_ms: self.t0.elapsed().as_millis() as u64,
                diagnostics: diagnostics.clone(),
            },
        );
        events.emit(Event::NodeFinished {
            key: key.clone(),
            status,
            exec_ms,
        });

        // If this node stood in for one that expanded, the original now has its answer, and its
        // dependents - which have been waiting on it all along - can proceed.
        //
        // The walk is a loop, not a single hop: a repair can itself be repaired, so the node that
        // finally produces an answer may be several substitutions below the node whose identity
        // it is carrying. Resolving only one level leaves the original node's dependents waiting
        // on something that will never settle, and the whole tail of the graph is reported as
        // skipped while the work it needed sits finished in the store.
        let mut resolved = key.clone();
        while let Some(parent) = self.delegated.remove(&resolved) {
            self.outputs.insert(parent.clone(), outputs.clone());
            if let Some(r) = self.reports.get_mut(&parent) {
                r.outputs = outputs.clone();
                if !diagnostics.is_empty() {
                    r.diagnostics = diagnostics.clone();
                }
                // A subgraph that fails fails the node that delegated to it. A subgraph that
                // succeeds leaves that node's status alone: how the delegating node itself was
                // obtained - ran, or came back from the memo - is a separate fact.
                if status == NodeStatus::Failed {
                    r.status = NodeStatus::Failed;
                }
            }
            match status {
                NodeStatus::Failed | NodeStatus::Skipped => self.skip_downstream(&parent, events),
                _ => self.unlock(&parent),
            }
            resolved = parent;
        }

        match status {
            NodeStatus::Failed | NodeStatus::Skipped => self.skip_downstream(&key, events),
            _ => self.unlock(&key),
        }
    }

    /// Decrement dependents and queue any that are now fully resolved.
    fn unlock(&mut self, key: &NodeKey) {
        let children = match self.dependents.get(key) {
            Some(c) => c.clone(),
            None => return,
        };
        for c in children {
            if let Some(n) = self.pending.get_mut(&c) {
                *n = n.saturating_sub(1);
                if *n == 0 && !self.settled.contains(&c) {
                    self.ready.push_back(c);
                }
            }
        }
    }

    /// A failure does not fail the task. Its dependents are unreachable and are marked skipped;
    /// independent branches keep running, because their results are still worth caching.
    fn skip_downstream(&mut self, failed: &NodeKey, events: &Events) {
        let mut queue: VecDeque<NodeKey> = self
            .dependents
            .get(failed)
            .cloned()
            .unwrap_or_default()
            .into();
        while let Some(k) = queue.pop_front() {
            if !self.settled.insert(k.clone()) {
                continue;
            }
            let kind = self
                .graph
                .get(&k)
                .map(|s| s.kind)
                .unwrap_or(flash_core::NodeKind::Data);
            self.reports.insert(
                k.clone(),
                NodeReport {
                    key: k.clone(),
                    kind,
                    action: None,
                    status: NodeStatus::Skipped,
                    outputs: Vec::new(),
                    exec_ms: 0,
                    queue_ms: 0,
                    attribution: Attribution::default(),
                    was_job: false,
                    finished_at_ms: self.t0.elapsed().as_millis() as u64,
                    diagnostics: Vec::new(),
                },
            );
            events.emit(Event::NodeSkipped {
                key: k.clone(),
                because: failed.clone(),
            });
            if let Some(children) = self.dependents.get(&k) {
                queue.extend(children.iter().cloned());
            }
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Convenience: run a task with default config against a store rooted at `root`.
pub async fn run_task(
    root: impl AsRef<std::path::Path>,
    task_id: &str,
    graph: &TaskGraph,
    executor: Arc<dyn NodeExecutor>,
) -> Result<RunReport, EngineError> {
    let store = Store::open(root)?;
    let engine = Engine::new(store, EngineConfig::default());
    engine.run(task_id, graph, executor, Events::none()).await
}

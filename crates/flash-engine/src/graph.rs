//! The task graph: what the planner emits and the scheduler consumes.
//!
//! A graph is declared in terms of *logical* node keys. Action keys do not appear here at all,
//! because they cannot be known until inputs have been resolved to content at execution time.
//! That is deliberate: it is what gives early cutoff.

use flash_core::{Digest, Env, Lane, NodeKey, NodeKind};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum GraphError {
    #[error("node {0} declared twice")]
    Duplicate(NodeKey),
    #[error("node {node} depends on {missing}, which is not in the graph")]
    MissingDep { node: NodeKey, missing: NodeKey },
    #[error("the graph has a cycle through {0}")]
    Cycle(NodeKey),
    #[error("node {0} depends on itself")]
    SelfDep(NodeKey),
}

/// One step. `op` is an opaque payload owned by the adapter; the engine only ever hashes it.
///
/// Serializable because an expansion (a node replaced by the subgraph it computed) is written to
/// the content store, so that a memo hit on a planner node can rebuild the subgraph it planned
/// without calling the planner again.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeSpec {
    pub key: NodeKey,
    pub kind: NodeKind,
    #[serde(with = "flash_core::hex_bytes")]
    pub op: Vec<u8>,
    pub env: Env,
    /// Dependencies in declared order. Order is semantic: it is hashed into the action key.
    pub deps: Vec<NodeKey>,
    pub lane: Lane,
}

impl NodeSpec {
    pub fn new(key: impl Into<String>, kind: NodeKind) -> Self {
        let lane = match kind {
            NodeKind::Plan | NodeKind::Op => Lane::model(),
            _ => Lane::cpu(),
        };
        NodeSpec {
            key: NodeKey::new(key),
            kind,
            op: Vec::new(),
            env: Env::deterministic("v0"),
            deps: Vec::new(),
            lane,
        }
    }

    /// A source node: no dependencies, its op *is* the content hash of an input file.
    ///
    /// This is how an input change enters the graph. Nothing else needs to know about files.
    pub fn source(key: impl Into<String>, content: Digest) -> Self {
        let mut n = NodeSpec::new(key, NodeKind::Data);
        n.op = content.as_bytes().to_vec();
        n
    }

    pub fn op(mut self, op: impl Into<Vec<u8>>) -> Self {
        self.op = op.into();
        self
    }

    pub fn dep(mut self, key: impl Into<String>) -> Self {
        self.deps.push(NodeKey::new(key));
        self
    }

    pub fn deps<I, S>(mut self, keys: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.deps.extend(keys.into_iter().map(NodeKey::new));
        self
    }

    pub fn env(mut self, env: Env) -> Self {
        self.env = env;
        self
    }

    pub fn lane(mut self, lane: Lane) -> Self {
        self.lane = lane;
        self
    }
}

/// A validated execution order plus the reverse edges the scheduler needs.
#[derive(Clone, Debug)]
pub struct Plan {
    pub topo: Vec<NodeKey>,
    pub dependents: HashMap<NodeKey, Vec<NodeKey>>,
    pub indegree: HashMap<NodeKey, usize>,
}

#[derive(Clone, Debug, Default)]
pub struct TaskGraph {
    nodes: BTreeMap<NodeKey, Arc<NodeSpec>>,
}

impl TaskGraph {
    pub fn new() -> Self {
        TaskGraph::default()
    }

    pub fn add(&mut self, spec: NodeSpec) -> Result<(), GraphError> {
        if spec.deps.contains(&spec.key) {
            return Err(GraphError::SelfDep(spec.key));
        }
        if self.nodes.contains_key(&spec.key) {
            return Err(GraphError::Duplicate(spec.key));
        }
        self.nodes.insert(spec.key.clone(), Arc::new(spec));
        Ok(())
    }

    /// Chainable form, for building graphs inline. Panics on a duplicate key, which is a
    /// programming error in the planner rather than a runtime condition.
    pub fn with(mut self, spec: NodeSpec) -> Self {
        self.add(spec).expect("duplicate node key");
        self
    }

    pub fn get(&self, key: &NodeKey) -> Option<&Arc<NodeSpec>> {
        self.nodes.get(key)
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn keys(&self) -> impl Iterator<Item = &NodeKey> {
        self.nodes.keys()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&NodeKey, &Arc<NodeSpec>)> {
        self.nodes.iter()
    }

    /// Add several nodes that may depend on each other, in any order.
    ///
    /// Used by expansion. Returns the batch in dependency order so the caller can append it to a
    /// topological order it is already holding.
    pub fn add_batch(&mut self, batch: Vec<NodeSpec>) -> Result<Vec<NodeKey>, GraphError> {
        let incoming: std::collections::BTreeSet<NodeKey> =
            batch.iter().map(|s| s.key.clone()).collect();
        for spec in &batch {
            if self.nodes.contains_key(&spec.key) {
                return Err(GraphError::Duplicate(spec.key.clone()));
            }
            if spec.deps.contains(&spec.key) {
                return Err(GraphError::SelfDep(spec.key.clone()));
            }
            for d in &spec.deps {
                if !self.nodes.contains_key(d) && !incoming.contains(d) {
                    return Err(GraphError::MissingDep {
                        node: spec.key.clone(),
                        missing: d.clone(),
                    });
                }
            }
        }

        // Order the batch internally: a node comes after any sibling it depends on.
        let mut remaining: Vec<NodeSpec> = batch;
        let mut placed: std::collections::BTreeSet<NodeKey> = Default::default();
        let mut order = Vec::with_capacity(remaining.len());
        while !remaining.is_empty() {
            let before = remaining.len();
            let mut next = Vec::new();
            for spec in remaining.into_iter() {
                let ready = spec
                    .deps
                    .iter()
                    .all(|d| !incoming.contains(d) || placed.contains(d));
                if ready {
                    placed.insert(spec.key.clone());
                    order.push(spec);
                } else {
                    next.push(spec);
                }
            }
            remaining = next;
            if remaining.len() == before {
                return Err(GraphError::Cycle(remaining[0].key.clone()));
            }
        }

        let keys = order.iter().map(|s| s.key.clone()).collect();
        for spec in order {
            self.nodes.insert(spec.key.clone(), Arc::new(spec));
        }
        Ok(keys)
    }

    /// Replace one node's spec. Used by tests and by warm reruns that change an input.
    pub fn replace(&mut self, spec: NodeSpec) {
        self.nodes.insert(spec.key.clone(), Arc::new(spec));
    }

    /// Check the graph and produce a deterministic execution order.
    ///
    /// Determinism matters: two runs of the same graph must dispatch in the same order, or the
    /// benchmark's per node attribution is comparing different things.
    pub fn validate(&self) -> Result<Plan, GraphError> {
        let mut dependents: HashMap<NodeKey, Vec<NodeKey>> = HashMap::new();
        let mut indegree: HashMap<NodeKey, usize> = HashMap::new();

        for (key, spec) in &self.nodes {
            indegree.entry(key.clone()).or_insert(0);
            for d in &spec.deps {
                if !self.nodes.contains_key(d) {
                    return Err(GraphError::MissingDep {
                        node: key.clone(),
                        missing: d.clone(),
                    });
                }
                dependents.entry(d.clone()).or_default().push(key.clone());
                *indegree.entry(key.clone()).or_insert(0) += 1;
            }
        }
        for v in dependents.values_mut() {
            v.sort();
            v.dedup();
        }

        // Kahn, fed from a sorted queue so the order is stable across runs.
        let mut degree = indegree.clone();
        let mut queue: VecDeque<NodeKey> = self
            .nodes
            .keys()
            .filter(|k| degree.get(*k).copied().unwrap_or(0) == 0)
            .cloned()
            .collect();
        let mut topo = Vec::with_capacity(self.nodes.len());
        while let Some(k) = queue.pop_front() {
            topo.push(k.clone());
            if let Some(children) = dependents.get(&k) {
                for c in children {
                    let e = degree.get_mut(c).expect("child is in the graph");
                    *e -= 1;
                    if *e == 0 {
                        queue.push_back(c.clone());
                    }
                }
            }
        }

        if topo.len() != self.nodes.len() {
            let stuck = self
                .nodes
                .keys()
                .find(|k| !topo.contains(k))
                .cloned()
                .expect("a node is missing from topo order");
            return Err(GraphError::Cycle(stuck));
        }

        Ok(Plan {
            topo,
            dependents,
            indegree,
        })
    }

    /// Every node reachable downstream of `roots`, roots included.
    ///
    /// This is the *structural* dirty set: the nodes that could be affected by a change. The
    /// scheduler recomputes strictly fewer than these, because early cutoff prunes any branch
    /// whose parent produced identical bytes. Tests compare the two on purpose.
    pub fn downstream_closure<'a, I>(&self, roots: I) -> std::collections::BTreeSet<NodeKey>
    where
        I: IntoIterator<Item = &'a NodeKey>,
    {
        let plan = match self.validate() {
            Ok(p) => p,
            Err(_) => return Default::default(),
        };
        let mut out = std::collections::BTreeSet::new();
        let mut queue: VecDeque<NodeKey> = roots.into_iter().cloned().collect();
        while let Some(k) = queue.pop_front() {
            if !out.insert(k.clone()) {
                continue;
            }
            if let Some(children) = plan.dependents.get(&k) {
                queue.extend(children.iter().cloned());
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain() -> TaskGraph {
        TaskGraph::new()
            .with(NodeSpec::source("a", Digest::of(b"a")))
            .with(NodeSpec::new("b", NodeKind::Op).dep("a"))
            .with(NodeSpec::new("c", NodeKind::Verify).dep("b"))
    }

    #[test]
    fn topo_order_respects_edges() {
        let plan = chain().validate().unwrap();
        let pos = |k: &str| plan.topo.iter().position(|n| n.as_str() == k).unwrap();
        assert!(pos("a") < pos("b"));
        assert!(pos("b") < pos("c"));
    }

    #[test]
    fn topo_order_is_stable_across_validations() {
        let g = chain();
        assert_eq!(g.validate().unwrap().topo, g.validate().unwrap().topo);
    }

    #[test]
    fn cycles_are_rejected() {
        let mut g = TaskGraph::new();
        g.add(NodeSpec::new("a", NodeKind::Op).dep("b")).unwrap();
        g.add(NodeSpec::new("b", NodeKind::Op).dep("a")).unwrap();
        assert!(matches!(g.validate(), Err(GraphError::Cycle(_))));
    }

    #[test]
    fn missing_deps_are_rejected() {
        let mut g = TaskGraph::new();
        g.add(NodeSpec::new("a", NodeKind::Op).dep("ghost"))
            .unwrap();
        assert!(matches!(g.validate(), Err(GraphError::MissingDep { .. })));
    }

    #[test]
    fn self_dependency_is_rejected() {
        let mut g = TaskGraph::new();
        assert!(matches!(
            g.add(NodeSpec::new("a", NodeKind::Op).dep("a")),
            Err(GraphError::SelfDep(_))
        ));
    }

    #[test]
    fn downstream_closure_is_transitive() {
        let g = chain();
        let closure = g.downstream_closure([&NodeKey::new("a")]);
        assert_eq!(closure.len(), 3);
        let from_b = g.downstream_closure([&NodeKey::new("b")]);
        assert_eq!(from_b.len(), 2);
    }
}

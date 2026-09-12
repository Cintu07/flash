//! Eta from history, never from a guess.
//!
//! Section 3.1: "eta for a task = critical path sum of per node p50 durations from history.
//! shown live and updated as nodes finish. new node ids with no history show unknown rather than
//! a guess."
//!
//! So an eta here has two parts, and the second one is not decoration: `known_ms` is the critical
//! path over nodes we have measured, and `unknown_nodes` counts the steps we have never run and
//! refuse to invent a number for. A client renders "~32s (+3 steps unknown)". Collapsing that to
//! a single confident number is how progress bars lose their users.
//!
//! Known limitation, stated rather than hidden: this is a pure critical path. It does not model
//! queueing at a lane cap, so a graph 40 nodes wide on a 4-slot lane will finish later than the
//! eta says. Fixing that means a scheduling simulation rather than a longest path, and it is the
//! first thing to revisit once real adapters make lane contention common.

use crate::graph::{Plan, TaskGraph};
use flash_core::NodeKey;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// What one node is expected to still cost.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeCost {
    /// Finished, or a memo hit. Costs nothing from here.
    Done,
    /// Measured: p50 from history, minus whatever has already elapsed if it is running.
    Known(u64),
    /// Never run before. Deliberately not estimated.
    Unknown,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Eta {
    /// Critical path over nodes with history, in milliseconds.
    pub known_ms: u64,
    /// How many remaining nodes have no history at all.
    pub unknown_nodes: usize,
}

impl Eta {
    pub fn is_complete(&self) -> bool {
        self.unknown_nodes == 0
    }

    pub fn render(&self) -> String {
        let secs = self.known_ms as f64 / 1000.0;
        match self.unknown_nodes {
            0 if self.known_ms == 0 => "done".to_string(),
            0 => format!("~{secs:.1}s"),
            n => format!("~{secs:.1}s (+{n} steps unknown)"),
        }
    }

    /// Signed error against an actual wall time, as a percentage. Only meaningful when the eta
    /// had no unknown steps; phase 0's exit criterion is this under 20 percent.
    pub fn error_pct(&self, actual_ms: u64) -> f64 {
        if actual_ms == 0 {
            return 0.0;
        }
        (self.known_ms as f64 - actual_ms as f64) / actual_ms as f64 * 100.0
    }
}

/// Longest path over what is left, using measured costs only.
pub fn estimate(plan: &Plan, graph: &TaskGraph, costs: &HashMap<NodeKey, NodeCost>) -> Eta {
    estimate_over(&plan.topo, graph, costs)
}

/// The same, over a topological order held by the scheduler. The scheduler's order grows when a
/// node expands into a subgraph, so it cannot hand over an immutable `Plan`.
pub fn estimate_over(
    topo: &[NodeKey],
    graph: &TaskGraph,
    costs: &HashMap<NodeKey, NodeCost>,
) -> Eta {
    let mut finish: HashMap<&NodeKey, u64> = HashMap::with_capacity(topo.len());
    let mut unknown = 0usize;

    for key in topo {
        let spec = match graph.get(key) {
            Some(s) => s,
            None => continue,
        };
        let start = spec
            .deps
            .iter()
            .filter_map(|d| finish.get(d).copied())
            .max()
            .unwrap_or(0);
        let cost = match costs.get(key).copied().unwrap_or(NodeCost::Unknown) {
            NodeCost::Done => 0,
            NodeCost::Known(ms) => ms,
            NodeCost::Unknown => {
                unknown += 1;
                0
            }
        };
        finish.insert(key, start + cost);
    }

    Eta {
        known_ms: finish.values().copied().max().unwrap_or(0),
        unknown_nodes: unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::NodeSpec;
    use flash_core::{Digest, NodeKind};

    fn graph() -> TaskGraph {
        // a -> b -> d, a -> c -> d. b is the slow branch.
        TaskGraph::new()
            .with(NodeSpec::source("a", Digest::of(b"a")))
            .with(NodeSpec::new("b", NodeKind::Op).dep("a"))
            .with(NodeSpec::new("c", NodeKind::Op).dep("a"))
            .with(NodeSpec::new("d", NodeKind::Verify).dep("b").dep("c"))
    }

    fn costs(pairs: &[(&str, NodeCost)]) -> HashMap<NodeKey, NodeCost> {
        pairs.iter().map(|(k, c)| (NodeKey::new(*k), *c)).collect()
    }

    #[test]
    fn eta_follows_the_critical_path_not_the_sum() {
        let g = graph();
        let plan = g.validate().unwrap();
        let c = costs(&[
            ("a", NodeCost::Known(100)),
            ("b", NodeCost::Known(500)),
            ("c", NodeCost::Known(50)),
            ("d", NodeCost::Known(200)),
        ]);
        // critical path a->b->d = 800, not 850 total.
        assert_eq!(estimate(&plan, &g, &c).known_ms, 800);
    }

    #[test]
    fn finished_nodes_drop_out_of_the_estimate() {
        let g = graph();
        let plan = g.validate().unwrap();
        let c = costs(&[
            ("a", NodeCost::Done),
            ("b", NodeCost::Done),
            ("c", NodeCost::Done),
            ("d", NodeCost::Known(200)),
        ]);
        assert_eq!(estimate(&plan, &g, &c).known_ms, 200);
    }

    #[test]
    fn nodes_without_history_are_counted_never_guessed() {
        let g = graph();
        let plan = g.validate().unwrap();
        let c = costs(&[
            ("a", NodeCost::Known(100)),
            ("b", NodeCost::Unknown),
            ("c", NodeCost::Known(50)),
            ("d", NodeCost::Unknown),
        ]);
        let eta = estimate(&plan, &g, &c);
        assert_eq!(eta.unknown_nodes, 2);
        assert!(!eta.is_complete());
        assert!(eta.render().contains("unknown"));
    }

    #[test]
    fn error_pct_is_signed() {
        let eta = Eta {
            known_ms: 800,
            unknown_nodes: 0,
        };
        assert!(eta.error_pct(1000) < 0.0, "an underestimate reads negative");
        assert!(eta.error_pct(400) > 0.0, "an overestimate reads positive");
    }
}

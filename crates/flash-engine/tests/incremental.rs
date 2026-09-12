//! Phase 0 exit criterion: "warm rerun recomputes exactly the dirty closure on 100 synthetic
//! graphs."
//!
//! "Exactly" is the whole test. Recomputing a superset is the bug every incremental system has
//! at first and nobody notices, because the answers are still correct and it is merely slow.
//! Recomputing a subset is worse: stale output that looks fresh. So the expected set is computed
//! by an oracle that simulates propagation independently of the engine, and the two must match
//! node for node.

mod support;

use flash_core::{Digest, NodeKey, NodeKind};
use flash_engine::{NodeSpec, TaskGraph};
use flash_stub::{Bucket, Rng, StubExecutor, StubOp, mutate_source, stub_node, synthetic_graph};
use std::collections::{BTreeSet, HashMap};
use support::{engine_at, run};

/// Independent model of what a change must touch.
///
/// A node's action key changes when its own op changed or when any dependency produced different
/// bytes. A node's *output* changes only if it recomputed and is not a constant node; a constant
/// node reruns and emits the same bytes, which is exactly the early cutoff case.
fn expected_recompute(graph: &TaskGraph, mutated: &NodeKey) -> BTreeSet<NodeKey> {
    let plan = graph.validate().expect("graph is valid");
    let mut action_changed: HashMap<NodeKey, bool> = HashMap::new();
    let mut output_changed: HashMap<NodeKey, bool> = HashMap::new();

    for key in &plan.topo {
        let spec = graph.get(key).expect("node exists");
        let is_mutated = key == mutated;
        let upstream_moved = spec
            .deps
            .iter()
            .any(|d| output_changed.get(d).copied().unwrap_or(false));
        let acted = is_mutated || upstream_moved;
        let constant = StubOp::decode(&spec.op)
            .map(|o| o.constant)
            .unwrap_or(false);
        action_changed.insert(key.clone(), acted);
        output_changed.insert(key.clone(), acted && !constant);
    }

    action_changed
        .into_iter()
        .filter(|(_, v)| *v)
        .map(|(k, _)| k)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn cold_then_hot_then_warm_on_one_hundred_synthetic_graphs() {
    let mut pruned_below_structural = 0usize;
    let mut total_structural = 0usize;
    let mut total_recomputed = 0usize;

    for seed in 0..100u64 {
        let dir = tempfile::tempdir().expect("tempdir");
        let engine = engine_at(dir.path());
        let exec = StubExecutor::instant();
        let mut syn = synthetic_graph(seed, 12 + (seed as usize % 20));
        let task = format!("synthetic-{seed}");

        // Cold: nothing cached, everything runs.
        let cold = run(&engine, &task, &syn.graph, exec.clone()).await;
        assert!(cold.ok(), "seed {seed}: cold run should be clean");
        assert_eq!(
            cold.computed(),
            syn.graph.len(),
            "seed {seed}: a cold run must compute every node"
        );
        assert_eq!(cold.hits(), 0, "seed {seed}: nothing can hit on a cold run");

        // Hot: identical inputs, zero model calls.
        exec.reset_calls();
        let hot = run(&engine, &task, &syn.graph, exec.clone()).await;
        assert!(
            hot.is_hot(),
            "seed {seed}: identical inputs must compute nothing, computed {}",
            hot.computed()
        );
        assert_eq!(
            exec.calls(),
            0,
            "seed {seed}: a hot run must not call the executor at all"
        );

        // Warm: one source changes.
        let mutated = NodeKey::new(syn.sources[(seed as usize) % syn.sources.len()].clone());
        mutate_source(&mut syn.graph, mutated.as_str(), &format!("changed-{seed}"));

        let expected = expected_recompute(&syn.graph, &mutated);
        let warm = run(&engine, &task, &syn.graph, exec.clone()).await;
        let actual = warm.computed_keys();

        assert_eq!(
            actual,
            expected,
            "seed {seed}: warm rerun did not recompute exactly the dirty closure\n\
             extra: {:?}\nmissing: {:?}",
            actual.difference(&expected).collect::<Vec<_>>(),
            expected.difference(&actual).collect::<Vec<_>>()
        );

        // The structural closure is an upper bound. Early cutoff should often beat it.
        let structural = syn.graph.downstream_closure([&mutated]);
        assert!(
            actual.is_subset(&structural),
            "seed {seed}: recomputed a node outside the structural closure"
        );
        total_structural += structural.len();
        total_recomputed += actual.len();
        if actual.len() < structural.len() {
            pruned_below_structural += 1;
        }
    }

    // Early cutoff is not incidental: with ~15 percent constant nodes it should bite on most
    // graphs. If this ever drops to zero, action keys have quietly become node-id keys again.
    assert!(
        pruned_below_structural >= 40,
        "early cutoff pruned only {pruned_below_structural}/100 graphs; \
         recomputed {total_recomputed} of {total_structural} structurally reachable nodes"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_constant_node_keeps_its_children_hot() {
    // The section 6 claim, in miniature: new csvs arrive, the data summary reruns and comes out
    // identical, and "methodology" is a hot hit with zero model calls.
    let dir = tempfile::tempdir().unwrap();
    let engine = engine_at(dir.path());
    let exec = StubExecutor::instant();

    let mut graph = TaskGraph::new()
        .with(NodeSpec::source("csv:q3", Digest::of(b"rows-v1")))
        .with(stub_node(
            "data:summary",
            NodeKind::Data,
            // Numbers round to the same summary regardless of the raw rows.
            StubOp::new("summary").constant().bucket(Bucket::Apply),
        ))
        .with(stub_node(
            "section:methodology",
            NodeKind::Op,
            StubOp::new("methodology").cost(40),
        ));
    // wire deps
    graph.replace(
        stub_node(
            "data:summary",
            NodeKind::Data,
            StubOp::new("summary").constant().bucket(Bucket::Apply),
        )
        .dep("csv:q3"),
    );
    graph.replace(
        stub_node(
            "section:methodology",
            NodeKind::Op,
            StubOp::new("methodology").cost(40),
        )
        .dep("data:summary"),
    );

    let cold = run(&engine, "quarterly", &graph, exec.clone()).await;
    assert_eq!(cold.computed(), 3);

    mutate_source(&mut graph, "csv:q3", "rows-v2");
    let warm = run(&engine, "quarterly", &graph, exec.clone()).await;

    assert_eq!(
        warm.computed_keys(),
        ["csv:q3", "data:summary"]
            .iter()
            .map(|k| NodeKey::new(*k))
            .collect::<BTreeSet<_>>(),
        "only the changed source and the summary should rerun"
    );
    assert_eq!(
        warm.node("section:methodology").unwrap().status,
        flash_engine::NodeStatus::Hit,
        "the section must stay hot: its input bytes did not move"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_prompt_version_bump_invalidates_only_nodes_that_used_it() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine_at(dir.path());
    let exec = StubExecutor::instant();

    let v1 = flash_core::Env::new("exec-small", "prompt-v1", "code-v1");
    let v2 = flash_core::Env::new("exec-small", "prompt-v2", "code-v1");

    let build = |env: flash_core::Env| {
        TaskGraph::new()
            .with(NodeSpec::source("file:lib.rs", Digest::of(b"fn main() {}")))
            .with(
                stub_node("op:edit", NodeKind::Op, StubOp::new("edit"))
                    .dep("file:lib.rs")
                    .env(env),
            )
            .with(
                stub_node("verify:lsp", NodeKind::Verify, StubOp::new("lsp"))
                    .dep("op:edit")
                    .env(flash_core::Env::deterministic("code-v1")),
            )
    };

    let cold = run(&engine, "edit", &build(v1.clone()), exec.clone()).await;
    assert_eq!(cold.computed(), 3);

    let bumped = run(&engine, "edit", &build(v2), exec.clone()).await;
    let recomputed = bumped.computed_keys();
    assert!(
        recomputed.contains(&NodeKey::new("op:edit")),
        "the node using the bumped prompt must rerun"
    );
    assert!(
        !recomputed.contains(&NodeKey::new("file:lib.rs")),
        "an unrelated source must not rerun"
    );
    // And the verify node does *not* rerun, which is the interesting half. The new prompt
    // produced byte-identical output, so the downstream action key never moved. A system that
    // keyed on node ids instead of content would have rebuilt the whole tail of this graph for
    // nothing.
    assert_eq!(
        bumped.node("verify:lsp").unwrap().status,
        flash_engine::NodeStatus::Hit,
        "identical output must cut off the invalidation"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn identical_work_across_two_tasks_shares_one_cache() {
    // d10, in miniature: the same node in two different tasks against one store. The second
    // task pays nothing. This is the mechanism a shared team cache rides on.
    let dir = tempfile::tempdir().unwrap();
    let engine = engine_at(dir.path());
    // This one measures real time, so the executor actually sleeps.
    let exec = StubExecutor::new();

    let graph = TaskGraph::new()
        .with(NodeSpec::source("input", Digest::of(b"same bytes")))
        .with(stub_node("work", NodeKind::Op, StubOp::new("work").cost(40)).dep("input"));

    let first = run(&engine, "task-a", &graph, exec.clone()).await;
    assert_eq!(first.computed(), 2);

    exec.reset_calls();
    let second = run(&engine, "task-b", &graph, exec.clone()).await;
    assert!(
        second.is_hot(),
        "a different task, the same content, no work"
    );
    assert_eq!(exec.calls(), 0);
    assert!(
        second.saved_ms > 0,
        "the report should say how much wall time the cache saved"
    );
}

#[test]
fn synthetic_graphs_are_reproducible_from_their_seed() {
    let a = synthetic_graph(7, 20);
    let b = synthetic_graph(7, 20);
    assert_eq!(a.keys, b.keys);
    assert_eq!(a.constants, b.constants);
    let mut rng = Rng::new(7);
    let mut rng2 = Rng::new(7);
    assert_eq!(rng.next_u64(), rng2.next_u64());
}

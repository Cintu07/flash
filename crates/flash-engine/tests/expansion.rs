//! Dynamic graphs: a node replaced by the subgraph it computes.
//!
//! Two things have to be true for this to be worth having, and they pull against each other:
//!
//! * a planner must be able to emit work that did not exist when the task started, and a failed
//!   verify must be able to emit a repair nobody planned;
//! * none of that may escape content addressing, or warm and hot stop meaning anything.
//!
//! So the tests below check both halves: the subgraph runs and its result reaches the original
//! node's dependents, *and* a second run of the same task calls neither the planner nor the
//! repair, because both were cached like ordinary work.

mod support;

use flash_core::{Digest, NodeKind, NodeOutput};
use flash_engine::{
    Engine, EngineConfig, Events, ExecCtx, ExecFuture, Expansion, NodeExecutor, NodeResult,
    NodeSpec, NodeStatus, TaskGraph,
};
use flash_store::Store;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use support::{engine_at, run};

/// Records which nodes actually executed, so "this was not called again" is checkable.
#[derive(Default)]
struct Trace {
    keys: Mutex<Vec<String>>,
    calls: AtomicUsize,
}

impl Trace {
    fn note(&self, key: &str) {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.keys.lock().unwrap().push(key.to_string());
    }

    fn ran(&self, key: &str) -> bool {
        self.keys.lock().unwrap().iter().any(|k| k == key)
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }

    fn reset(&self) {
        self.keys.lock().unwrap().clear();
        self.calls.store(0, Ordering::Relaxed);
    }
}

/// An executor driven by the op payload, so each test can describe behaviour inline.
///
/// * `plain:<text>`      - emit a digest of the text and the inputs
/// * `plan:<n>`          - expand into an n-node chain and delegate to its tail
/// * `verify:strict`     - fail, but expand into fix-then-recheck and delegate to the recheck
/// * `verify:relaxed`    - pass
/// * `loop`              - expand into another `loop` node forever
fn scripted(trace: Arc<Trace>) -> Arc<dyn NodeExecutor> {
    let f = move |ctx: ExecCtx| -> ExecFuture {
        let trace = trace.clone();
        Box::pin(async move {
            let key = ctx.spec.key.as_str().to_string();
            let op = String::from_utf8_lossy(&ctx.spec.op).to_string();
            trace.note(&key);

            let mut h = flash_core::Hasher::new("test.exec");
            h.str(&op);
            for d in &ctx.inputs {
                h.digest(d);
            }
            let out = h.finish();

            if let Some(n) = op.strip_prefix("plan:") {
                let n: usize = n.parse().unwrap_or(1);
                let mut nodes = Vec::new();
                for i in 0..n {
                    // Relative names: the engine splices these in under the planner's own key.
                    let mut spec = NodeSpec::new(format!("step{i}"), NodeKind::Op)
                        .op(format!("plain:{key}-step{i}").into_bytes());
                    // Chain them, with the first also carrying the planner's own inputs.
                    if i == 0 {
                        for d in &ctx.spec.deps {
                            spec = spec.dep(d.as_str());
                        }
                    } else {
                        spec = spec.dep(format!("step{}", i - 1));
                    }
                    nodes.push(spec);
                }
                let tail = format!("step{}", n - 1);
                return NodeResult::expand(vec![out], Expansion::new(nodes, tail));
            }

            if op == "verify:strict" {
                // The rung failed. Emit the repair and the recheck, and hand identity to the
                // recheck: whatever it decides is this node's answer.
                let diagnostics = ["E0308: mismatched types at line 12".to_string()];
                let diag_digest = Digest::of(diagnostics.join("\n").as_bytes());
                let fix = NodeSpec::new("fix", NodeKind::Op)
                    // The diagnostics are part of the op, so an identical failure produces an
                    // identical action key, and the repair is cacheable.
                    .op(format!("plain:fix-for-{}", diag_digest.short()).into_bytes())
                    .deps(ctx.spec.deps.iter().map(|d| d.as_str().to_string()));
                let recheck = NodeSpec::new("recheck", NodeKind::Verify)
                    .op(b"verify:relaxed".to_vec())
                    .dep("fix");
                return NodeResult::expand(
                    vec![out],
                    Expansion::new(vec![fix, recheck], "recheck"),
                );
            }

            if op == "loop" {
                let next = NodeSpec::new("again", NodeKind::Op).op(b"loop".to_vec());
                return NodeResult::expand(vec![out], Expansion::new(vec![next], "again"));
            }

            NodeOutput::pass(vec![out]).into()
        })
    };
    Arc::new(f)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_planner_emits_work_that_did_not_exist_and_its_consumer_waits_for_it() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine_at(dir.path());
    let trace = Arc::new(Trace::default());

    let graph = TaskGraph::new()
        .with(NodeSpec::source("brief", Digest::of(b"write me a report")))
        .with(
            NodeSpec::new("plan", NodeKind::Plan)
                .op(b"plan:3".to_vec())
                .dep("brief"),
        )
        // The consumer was written against "plan" and knows nothing about what plan emits.
        .with(
            NodeSpec::new("publish", NodeKind::Data)
                .op(b"plain:publish".to_vec())
                .dep("plan"),
        );

    let report = run(&engine, "planning", &graph, scripted(trace.clone())).await;

    assert!(report.ok(), "the expanded run should finish clean");
    assert_eq!(
        report.node("plan").unwrap().status,
        NodeStatus::Expanded,
        "the planner delegated rather than producing the answer itself"
    );
    for i in 0..3 {
        assert!(
            trace.ran(&format!("plan/step{i}")),
            "planned step {i} should have run"
        );
    }

    // The consumer's inputs are the substitute's outputs, not the planner's.
    let tail_out = report.node("plan/step2").unwrap().outputs.clone();
    assert_eq!(report.node("plan").unwrap().outputs, tail_out);
    assert!(
        report.node("publish").unwrap().finished_at_ms
            >= report.node("plan/step2").unwrap().finished_at_ms,
        "the consumer must wait for the subgraph, not for the planner"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_run_replays_the_plan_without_calling_the_planner() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine_at(dir.path());
    let trace = Arc::new(Trace::default());

    let graph = TaskGraph::new()
        .with(NodeSpec::source("brief", Digest::of(b"same brief")))
        .with(
            NodeSpec::new("plan", NodeKind::Plan)
                .op(b"plan:2".to_vec())
                .dep("brief"),
        )
        .with(
            NodeSpec::new("publish", NodeKind::Data)
                .op(b"plain:publish".to_vec())
                .dep("plan"),
        );

    let first = run(&engine, "planning", &graph, scripted(trace.clone())).await;
    assert!(first.ok());
    assert!(trace.calls() > 0);

    trace.reset();
    let second = run(&engine, "planning", &graph, scripted(trace.clone())).await;

    assert_eq!(
        trace.calls(),
        0,
        "a hot run must not call the planner or anything it planned; ran: {:?}",
        trace.keys.lock().unwrap()
    );
    assert!(second.ok());
    assert_eq!(
        second.node("plan/step1").unwrap().status,
        NodeStatus::Hit,
        "the planned subgraph must come back from the cache, not be re-planned"
    );
    assert_eq!(
        second.node("publish").unwrap().outputs,
        first.node("publish").unwrap().outputs,
        "replaying a cached plan must reproduce the same result"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_rung_repairs_itself_and_the_repair_is_cached() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine_at(dir.path());
    let trace = Arc::new(Trace::default());

    let graph = TaskGraph::new()
        .with(NodeSpec::source("file", Digest::of(b"fn main() {}")))
        .with(
            NodeSpec::new("edit", NodeKind::Op)
                .op(b"plain:edit".to_vec())
                .dep("file"),
        )
        .with(
            NodeSpec::new("verify", NodeKind::Verify)
                .op(b"verify:strict".to_vec())
                .dep("edit"),
        )
        .with(
            NodeSpec::new("commit", NodeKind::Data)
                .op(b"plain:commit".to_vec())
                .dep("verify"),
        );

    let first = run(&engine, "repair", &graph, scripted(trace.clone())).await;

    assert!(
        first.ok(),
        "the task should succeed through repair, not fail at the rung"
    );
    assert!(trace.ran("verify/fix"), "a repair node should have run");
    assert!(trace.ran("verify/recheck"), "the recheck should have run");
    assert_eq!(first.node("verify").unwrap().status, NodeStatus::Expanded);
    assert_eq!(
        first.node("commit").unwrap().status,
        NodeStatus::Computed,
        "downstream work proceeds once the repair passes"
    );

    // Same failure, same diagnostics, same repair: all of it cached.
    trace.reset();
    let second = run(&engine, "repair", &graph, scripted(trace.clone())).await;
    assert_eq!(
        trace.calls(),
        0,
        "a repeated task must not redo the repair; ran: {:?}",
        trace.keys.lock().unwrap()
    );
    assert!(second.is_hot());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_expansion_loop_is_stopped_rather_than_left_to_burn_tokens() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let engine = Engine::new(
        store,
        EngineConfig {
            max_expansion_depth: 3,
            ..Default::default()
        },
    );
    let trace = Arc::new(Trace::default());

    let graph = TaskGraph::new().with(NodeSpec::new("spin", NodeKind::Op).op(b"loop".to_vec()));

    let err = engine
        .run("spin", &graph, scripted(trace.clone()), Events::none())
        .await
        .expect_err("an endless repair loop must stop the run");

    assert!(
        matches!(err, flash_engine::EngineError::ExpansionDepth(_)),
        "expected a depth limit, got {err}"
    );
    assert!(
        trace.calls() <= 4,
        "the loop should have been cut off quickly, ran {} times",
        trace.calls()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_expansion_that_does_not_contain_its_substitute_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine_at(dir.path());

    let bad = move |_ctx: ExecCtx| -> ExecFuture {
        Box::pin(async move {
            NodeResult::expand(
                vec![Digest::of(b"x")],
                Expansion::new(
                    vec![NodeSpec::new("real", NodeKind::Op).op(b"plain:real".to_vec())],
                    "does-not-exist",
                ),
            )
        })
    };

    let graph = TaskGraph::new().with(NodeSpec::new("n", NodeKind::Plan).op(b"x".to_vec()));
    let err = engine
        .run("bad", &graph, Arc::new(bad), Events::none())
        .await
        .expect_err("a malformed expansion must not be spliced in");
    assert!(matches!(
        err,
        flash_engine::EngineError::BadExpansion { .. }
    ));
}

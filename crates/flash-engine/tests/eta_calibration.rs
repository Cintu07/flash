//! Phase 0 exit criterion: "eta error under 20 percent on graphs with history."
//!
//! This is only a real test if the stubs have a tail. A stub that always takes exactly 40 ms
//! makes any estimator look calibrated. These stubs draw around their median and go 2x to 4x
//! long about one run in fourteen, which is roughly how a render or a test suite behaves, and
//! it is what a p50-based eta is supposed to absorb.
//!
//! Every run here mutates its source, so nothing is ever served from the memo. The eta is being
//! judged on predicting real work, not on predicting cache hits.

mod support;

use flash_core::{Digest, NodeKind};
use flash_engine::{NodeSpec, TaskGraph};
use flash_stub::{StubExecutor, StubOp, mutate_source, stub_node};
use support::{engine_at, run};

/// A shape close to the section 6 report: one source, a summary, three sections in parallel,
/// then an assemble step. Width stays under the lane cap so queueing is not what is being
/// measured; that limitation is documented on `eta::estimate` rather than hidden here.
fn report_graph(marker: &str) -> TaskGraph {
    // Node costs are large relative to scheduler overhead and os jitter on purpose: at 5 ms a
    // node this would be measuring the runtime's own overhead rather than the estimator.
    let mut g = TaskGraph::new()
        .with(NodeSpec::source("csv", Digest::of(marker.as_bytes())))
        .with(stub_node("summary", NodeKind::Data, StubOp::new("summary").cost(60)).dep("csv"));
    for s in ["intro", "results", "outlook"] {
        g = g.with(
            stub_node(
                format!("section:{s}"),
                NodeKind::Op,
                StubOp::new(s).cost(90),
            )
            .dep("summary"),
        );
    }
    g.with(
        stub_node("assemble", NodeKind::Data, StubOp::new("assemble").cost(50))
            .deps(["section:intro", "section:results", "section:outlook"])
            .dep("summary"),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn eta_from_history_lands_within_twenty_percent() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine_at(dir.path());
    let exec = StubExecutor::new();

    // Warm up history. Each run sees a changed input, so every node genuinely runs.
    for i in 0..12 {
        let g = report_graph(&format!("rows-{i}"));
        let r = run(&engine, "report", &g, exec.clone()).await;
        assert_eq!(r.computed(), g.len(), "warmup run {i} must be fully cold");
    }

    // Measure. This is a wall-clock test, so it is judged on the median of several runs rather
    // than on any one: a single run that lands next to a busy cpu says nothing about the
    // estimator. History is a rolling window, so sustained load corrects itself within a few
    // runs; only a sudden change in load shows up here, and the median absorbs that.
    let mut errors: Vec<f64> = Vec::new();
    for i in 100..109 {
        let mut g = report_graph("rows-final");
        mutate_source(&mut g, "csv", &format!("rows-{i}"));
        let r = run(&engine, "report", &g, exec.clone()).await;
        assert!(
            r.eta_at_start.is_complete(),
            "every node has history by now, so the eta must have no unknown steps"
        );
        errors.push(r.eta_at_start.error_pct(r.wall_ms).abs());
    }

    errors.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = errors[errors.len() / 2];
    assert!(
        median < 20.0,
        "median eta error {median:.1} percent, samples {errors:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_graph_with_no_history_reports_unknown_rather_than_a_number() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine_at(dir.path());
    let exec = StubExecutor::instant();

    let g = report_graph("first-ever");
    let r = run(&engine, "report", &g, exec).await;

    assert_eq!(
        r.eta_at_start.known_ms, 0,
        "with no history there is nothing to add up"
    );
    assert_eq!(
        r.eta_at_start.unknown_nodes,
        g.len(),
        "every node should be counted as unknown, not estimated"
    );
    assert!(r.eta_at_start.render().contains("unknown"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cached_task_predicts_near_zero() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine_at(dir.path());
    let exec = StubExecutor::new();

    let g = report_graph("stable");
    let cold = run(&engine, "report", &g, exec.clone()).await;
    let hot = run(&engine, "report", &g, exec.clone()).await;

    assert!(hot.is_hot());
    assert!(
        hot.wall_ms * 4 < cold.wall_ms.max(1),
        "a hot run should be a different order of magnitude: hot {} ms vs cold {} ms",
        hot.wall_ms,
        cold.wall_ms
    );
    assert!(
        hot.saved_ms > hot.wall_ms,
        "the run should report saving more time than it spent"
    );
}

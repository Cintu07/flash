//! Section 3.2: slow steps are jobs, the orchestrator never blocks on one, jobs survive a
//! disconnect, and a crash resumes at the graph frontier.

mod support;

use flash_core::{Digest, NodeKind};
use flash_engine::{Engine, Event, Events, NodeSpec, NodeStatus, TaskGraph};
use flash_stub::{Bucket, StubExecutor, StubOp, stub_node};
use std::sync::Arc;
use std::time::Duration;
use support::{engine_at, run};

/// A render (slow by nature) alongside fast work that does not depend on it.
fn render_graph(cost_ms: u64) -> TaskGraph {
    let mut g = TaskGraph::new()
        .with(NodeSpec::source("doc", Digest::of(b"blocks")))
        .with(
            stub_node(
                "render:pdf",
                NodeKind::Render,
                StubOp::new("render").cost(cost_ms).bucket(Bucket::Render),
            )
            .dep("doc"),
        );
    for i in 0..4 {
        g = g.with(
            stub_node(
                format!("section:{i}"),
                NodeKind::Op,
                StubOp::new(format!("section {i}")).cost(15),
            )
            .dep("doc"),
        );
    }
    g
}

#[tokio::test(flavor = "multi_thread")]
async fn a_render_is_a_job_and_does_not_block_the_rest_of_the_graph() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine_at(dir.path());
    let (events, mut rx) = Events::channel();

    let graph = render_graph(300);
    let report = engine
        .run("doc-task", &graph, StubExecutor::new(), events)
        .await
        .unwrap();

    let render = report.node("render:pdf").unwrap();
    assert!(
        render.was_job,
        "a render is a job by kind, before it has any history at all"
    );

    // The sections are independent of the render and must not wait for it.
    for i in 0..4 {
        let s = report.node(&format!("section:{i}")).unwrap();
        assert!(
            s.finished_at_ms < render.finished_at_ms,
            "section {i} finished at {} ms, after the render at {} ms: the scheduler blocked",
            s.finished_at_ms,
            render.finished_at_ms
        );
    }

    // Time to first visible change is not the time to finish.
    let first = report.first_change_ms.expect("something was produced");
    assert!(
        first < render.finished_at_ms,
        "first change at {first} ms should precede the render finishing"
    );

    let mut progress_seen = 0;
    while let Ok(ev) = rx.try_recv() {
        if let Event::JobProgress { key, .. } = ev
            && key.as_str() == "render:pdf"
        {
            progress_seen += 1;
        }
    }
    assert!(
        progress_seen > 0,
        "a job must stream progress, not just finish"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_job_fails_and_is_never_cached() {
    let dir = tempfile::tempdir().unwrap();
    let engine: Arc<Engine> = engine_at(dir.path());
    let graph = render_graph(4_000);

    let runner = {
        let engine = engine.clone();
        let graph = graph.clone();
        tokio::spawn(async move {
            engine
                .run("cancel-task", &graph, StubExecutor::new(), Events::none())
                .await
                .unwrap()
        })
    };

    // Wait for the render to register, then cancel it the way a client would.
    let mut cancelled = false;
    for _ in 0..200 {
        if let Some((id, key)) = engine.jobs().live().first().cloned() {
            assert_eq!(key.as_str(), "render:pdf");
            assert!(engine.jobs().cancel(id));
            cancelled = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(cancelled, "the render should have registered as a live job");

    let report = runner.await.unwrap();
    assert_eq!(
        report.node("render:pdf").unwrap().status,
        NodeStatus::Failed
    );

    // A cancelled node must not poison the cache: rerunning recomputes it.
    let rerun = run(&engine, "cancel-task", &graph, StubExecutor::instant()).await;
    assert!(
        rerun
            .computed_keys()
            .contains(&flash_core::NodeKey::new("render:pdf")),
        "a cancelled job must not have been memoized"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_crash_resumes_at_the_frontier_not_from_zero() {
    let dir = tempfile::tempdir().unwrap();
    let graph = {
        // A chain, so the frontier after a crash is unambiguous.
        let mut g = TaskGraph::new().with(NodeSpec::source("src", Digest::of(b"x")));
        let mut prev = "src".to_string();
        for i in 0..8 {
            let key = format!("step:{i}");
            g = g.with(
                stub_node(key.clone(), NodeKind::Op, StubOp::new(key.clone()).cost(60))
                    .dep(prev.clone()),
            );
            prev = key;
        }
        g
    };

    // Crash: drop the run future partway through. Completed nodes are already journalled.
    {
        let engine = engine_at(dir.path());
        let fut = engine.run("long", &graph, StubExecutor::new(), Events::none());
        let _ = tokio::time::timeout(Duration::from_millis(200), fut).await;
    }

    // Remove the memo store so the only thing that can save work is the journal itself.
    std::fs::remove_dir_all(dir.path().join("memo")).unwrap();

    let engine = engine_at(dir.path());
    let report = run(&engine, "long", &graph, StubExecutor::instant()).await;
    let resumed = report.count(NodeStatus::Resumed);
    assert!(
        resumed >= 1,
        "nothing resumed: the journal did not record the frontier"
    );
    assert!(
        resumed < graph.len(),
        "everything resumed: the crash did not actually interrupt anything"
    );
    assert!(report.ok(), "the resumed run should still finish the task");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failure_skips_its_dependents_and_spares_everything_else() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine_at(dir.path());

    let graph = TaskGraph::new()
        .with(NodeSpec::source("src", Digest::of(b"x")))
        .with(stub_node("edit", NodeKind::Op, StubOp::new("edit")).dep("src"))
        .with(
            stub_node(
                "verify",
                NodeKind::Verify,
                StubOp::new("verify").failing().bucket(Bucket::Verify),
            )
            .dep("edit"),
        )
        .with(stub_node("render", NodeKind::Render, StubOp::new("render")).dep("verify"))
        // An independent branch: its result is still worth computing and caching.
        .with(stub_node("unrelated", NodeKind::Op, StubOp::new("unrelated")).dep("src"));

    let report = run(&engine, "failing", &graph, StubExecutor::instant()).await;

    assert_eq!(report.node("verify").unwrap().status, NodeStatus::Failed);
    assert_eq!(report.node("render").unwrap().status, NodeStatus::Skipped);
    assert_eq!(
        report.node("unrelated").unwrap().status,
        NodeStatus::Computed
    );
    assert!(!report.ok());
    assert!(
        !report.node("verify").unwrap().diagnostics.is_empty(),
        "a failure must carry structured diagnostics back for the model to act on"
    );

    // Rerun: the failure is not cached, the rest is.
    let rerun = run(&engine, "failing", &graph, StubExecutor::instant()).await;
    assert_eq!(rerun.node("edit").unwrap().status, NodeStatus::Hit);
    assert_eq!(rerun.node("verify").unwrap().status, NodeStatus::Failed);
}

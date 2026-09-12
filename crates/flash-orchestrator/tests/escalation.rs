//! The escalation ladder from section 4.1, end to end.
//!
//! ops -> ops with diagnostics -> one unified diff -> give up.
//!
//! Every step is a node, so the interesting assertions are not just "it recovered" but "it
//! recovered and the recovery is cached, and the fallback rate was counted". The fallback rate is
//! phase 1's kill line: if entity ops cannot express what a language needs, the schema is wrong
//! and no amount of retrying fixes it, so the number has to be visible rather than buried.

use flash_adapter_code::CodeAdapter;
use flash_orchestrator::model::ScriptedModel;
use flash_orchestrator::{Runtime, RuntimeConfig, Task, result_for};
use flash_store::Store;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;

const FILE: &str = "fn answer() -> u32 {\n    41\n}\n";

fn write(dir: &Path, name: &str, body: &str) {
    std::fs::write(dir.join(name), body).unwrap();
}

fn planner() -> ScriptedModel {
    ScriptedModel::new().otherwise(json!({
        "edits": [{
            "path": "lib.rs",
            "target": "fn:answer",
            "instruction": "return 42"
        }]
    }))
}

fn runtime(dir: &Path, executor: ScriptedModel) -> Arc<Runtime> {
    Runtime::new(
        vec![Arc::new(CodeAdapter::new())],
        Arc::new(planner()),
        Arc::new(executor),
        RuntimeConfig {
            workspace: Some(dir.to_path_buf()),
            levels: vec![0, 1],
            ..Default::default()
        },
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rejected_op_is_retried_with_the_diagnostics_and_then_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "lib.rs", FILE);

    // First attempt names an entity that does not exist; the adapter refuses it. The retry sees
    // the rejection in its prompt and gets it right.
    let executor = ScriptedModel::new()
        .on(
            "# diagnostics",
            json!([{"op":"replace_body","entity":"fn:answer","body":"42"}]),
        )
        .otherwise(json!([{"op":"replace_body","entity":"fn:does_not_exist","body":"42"}]));

    let rt = runtime(dir.path(), executor);
    let report = Task::new("make answer return 42")
        .root(dir.path())
        .file("lib.rs")
        .id("escalate")
        .run(dir.path().join(".flash"), rt.clone())
        .await
        .unwrap();

    assert!(report.ok(), "the task should recover through a repair");

    let store = Store::open(dir.path().join(".flash")).unwrap();
    let digest = result_for(&report, "lib.rs").expect("a result");
    let text = String::from_utf8(store.content.get(digest).unwrap()).unwrap();
    assert!(text.contains("42"), "{text}");

    let m = rt.metrics.snapshot();
    assert_eq!(
        m.ops_rejected, 1,
        "the first attempt must be counted as a rejection"
    );
    assert_eq!(m.ops_applied, 1);
    assert_eq!(m.repairs, 1);
    assert_eq!(
        m.diff_fallbacks, 0,
        "ops recovered, so no fallback was needed"
    );
    assert!((m.op_resolve_rate() - 0.5).abs() < 1e-9, "{:?}", m);
}

#[tokio::test(flavor = "multi_thread")]
async fn ops_failing_twice_falls_back_to_a_unified_diff() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "lib.rs", FILE);

    // Ops always fail. The third attempt is the diff, which the fixture answers by matching on
    // the diff system prompt.
    let executor = ScriptedModel::new()
        .on(
            "emit one unified diff",
            json!([{
                "op": "unified_diff",
                "diff": "@@ -1,3 +1,3 @@\n fn answer() -> u32 {\n-    41\n+    42\n }\n"
            }]),
        )
        .otherwise(json!([{"op":"replace_body","entity":"fn:nope","body":"42"}]));

    let rt = runtime(dir.path(), executor);
    let report = Task::new("make answer return 42")
        .root(dir.path())
        .file("lib.rs")
        .id("fallback")
        .run(dir.path().join(".flash"), rt.clone())
        .await
        .unwrap();

    assert!(
        report.ok(),
        "the diff fallback should carry the edit; nodes: {:?}",
        report
            .nodes
            .iter()
            .map(|n| (n.key.as_str().to_string(), n.status, n.diagnostics.clone()))
            .collect::<Vec<_>>()
    );

    let store = Store::open(dir.path().join(".flash")).unwrap();
    let digest = result_for(&report, "lib.rs").expect("a result");
    let text = String::from_utf8(store.content.get(digest).unwrap()).unwrap();
    assert!(text.contains("42"), "{text}");

    let m = rt.metrics.snapshot();
    assert_eq!(m.diff_fallbacks, 1, "the fallback must be counted: {m:?}");
    assert_eq!(m.ops_rejected, 2, "ops were tried twice before the diff");
    assert!(m.fallback_rate() > 0.0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_model_that_never_gets_it_right_fails_the_task_rather_than_looping() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "lib.rs", FILE);

    let executor = ScriptedModel::new()
        .otherwise(json!([{"op":"replace_body","entity":"fn:nope","body":"x"}]));

    let rt = runtime(dir.path(), executor);
    let report = Task::new("make answer return 42")
        .root(dir.path())
        .file("lib.rs")
        .id("hopeless")
        .run(dir.path().join(".flash"), rt.clone())
        .await
        .unwrap();

    assert!(!report.ok(), "an unfixable edit must fail the task");
    let m = rt.metrics.snapshot();
    assert_eq!(m.gave_up, 1, "it must stop, not keep paying for attempts");
    assert!(
        m.edits_attempted <= 3,
        "section 4.1 allows ops, ops, diff: {m:?}"
    );

    // And nothing broken was cached: a rerun tries again rather than replaying a failure.
    let rt2 = runtime(
        dir.path(),
        ScriptedModel::new()
            .otherwise(json!([{"op":"replace_body","entity":"fn:nope","body":"x"}])),
    );
    let again = Task::new("make answer return 42")
        .root(dir.path())
        .file("lib.rs")
        .id("hopeless")
        .run(dir.path().join(".flash"), rt2.clone())
        .await
        .unwrap();
    assert!(!again.ok());
    assert!(
        rt2.metrics.snapshot().edits_attempted > 0,
        "a failed edit must not be served from the cache"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repair_is_cached_so_the_second_run_of_a_repaired_task_is_hot() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "lib.rs", FILE);

    let make = || {
        ScriptedModel::new()
            .on(
                "# diagnostics",
                json!([{"op":"replace_body","entity":"fn:answer","body":"42"}]),
            )
            .otherwise(json!([{"op":"replace_body","entity":"fn:missing","body":"42"}]))
    };

    let first = runtime(dir.path(), make());
    let r1 = Task::new("make answer return 42")
        .root(dir.path())
        .file("lib.rs")
        .id("repair-cache")
        .run(dir.path().join(".flash"), first.clone())
        .await
        .unwrap();
    assert!(r1.ok());
    assert_eq!(first.metrics.snapshot().repairs, 1);

    let second = runtime(dir.path(), make());
    let r2 = Task::new("make answer return 42")
        .root(dir.path())
        .file("lib.rs")
        .id("repair-cache")
        .run(dir.path().join(".flash"), second.clone())
        .await
        .unwrap();

    assert!(r2.ok());
    assert_eq!(
        second.metrics.snapshot().edits_attempted,
        0,
        "the repaired path must replay from the cache, including the repair itself"
    );
    assert!(
        r2.is_hot(),
        "a repaired task reruns hot: {:?}",
        r2.nodes.len()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lint_failure_produces_a_fix_and_a_recheck() {
    let dir = tempfile::tempdir().unwrap();
    // A caller that depends on the function being edited, so deleting it breaks the file in a way
    // rung 1 can see without a compiler.
    write(
        dir.path(),
        "lib.rs",
        "fn answer() -> u32 {\n    41\n}\n\nfn describe() -> String {\n    format!(\"{}\", answer())\n}\n",
    );

    // The first edit deletes `answer` outright, leaving `describe` calling a function that is no
    // longer there. Rung 1 catches it; the repair puts it back.
    let executor = ScriptedModel::new()
        .on(
            "dangling-reference",
            json!([{
                "op": "insert_after",
                "entity": "fn:describe",
                "code": "fn answer() -> u32 {\n    42\n}"
            }]),
        )
        .otherwise(json!([{"op":"delete","entity":"fn:answer"}]));

    let rt = runtime(dir.path(), executor);
    let report = Task::new("make answer return 42")
        .root(dir.path())
        .file("lib.rs")
        .id("lint-repair")
        .run(dir.path().join(".flash"), rt.clone())
        .await
        .unwrap();

    let m = rt.metrics.snapshot();
    assert!(
        m.rungs_failed >= 1,
        "rung 1 should have caught the dangling reference: {m:?}"
    );
    assert!(
        m.repairs >= 1,
        "the failed rung should have produced a repair: {m:?}"
    );
    assert!(report.ok(), "the recheck should pass after the repair");

    let store = Store::open(dir.path().join(".flash")).unwrap();
    let digest = result_for(&report, "lib.rs").expect("a result");
    let text = String::from_utf8(store.content.get(digest).unwrap()).unwrap();
    assert!(
        text.contains("fn answer()"),
        "the repair should have restored the function: {text}"
    );
    assert!(text.contains("42"), "{text}");
}

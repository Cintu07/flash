//! flash-orchestrator: the layer that turns a request into a graph and runs it.
//!
//! The engine knows about nodes and hashes. The adapters know about functions and blocks. This
//! crate is the only place that knows both, plus the two models (d3: planner big, executor small,
//! one protocol, no training).
//!
//! A task is three lines of setup and one graph:
//!
//! ```no_run
//! # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! use flash_orchestrator::{Task, Runtime, RuntimeConfig};
//! use flash_orchestrator::model::ScriptedModel;
//! use flash_adapter_code::CodeAdapter;
//! use std::sync::Arc;
//!
//! let runtime = Runtime::new(
//!     vec![Arc::new(CodeAdapter::new())],
//!     Arc::new(ScriptedModel::new()),
//!     Arc::new(ScriptedModel::new()),
//!     RuntimeConfig::default(),
//! );
//!
//! let report = Task::new("add bounds checking to the parser")
//!     .file("src/lib.rs")
//!     .run(".flash", runtime)
//!     .await?;
//!
//! println!("{} computed, {} hit", report.computed(), report.hits());
//! # Ok(()) }
//! ```

pub mod exec;
pub mod model;
pub mod nodes;
pub mod prompts;

pub use exec::{MetricsSnapshot, Runtime, RuntimeConfig, RuntimeMetrics};
pub use nodes::{EditRequest, NodeOp};

use flash_core::{Digest, Env, NodeKind};
use flash_engine::{Engine, EngineConfig, EngineError, Events, NodeSpec, RunReport, TaskGraph};
use flash_store::Store;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum TaskError {
    #[error(transparent)]
    Engine(#[from] EngineError),
    #[error(transparent)]
    Store(#[from] flash_store::StoreError),
    #[error("could not read {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("a task needs at least one file")]
    NoFiles,
}

/// A request: an instruction and the files it may touch.
pub struct Task {
    instruction: String,
    files: Vec<PathBuf>,
    root: Option<PathBuf>,
    task_id: Option<String>,
    events: Events,
}

impl Task {
    pub fn new(instruction: impl Into<String>) -> Self {
        Task {
            instruction: instruction.into(),
            files: Vec::new(),
            root: None,
            task_id: None,
            events: Events::none(),
        }
    }

    pub fn file(mut self, path: impl Into<PathBuf>) -> Self {
        self.files.push(path.into());
        self
    }

    pub fn files<I, P>(mut self, paths: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        self.files.extend(paths.into_iter().map(Into::into));
        self
    }

    /// Where the files live. Rungs that shell out run here.
    pub fn root(mut self, root: impl Into<PathBuf>) -> Self {
        self.root = Some(root.into());
        self
    }

    /// Stable id for this task, so an interrupted run resumes at its frontier.
    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.task_id = Some(id.into());
        self
    }

    pub fn events(mut self, events: Events) -> Self {
        self.events = events;
        self
    }

    /// Build the graph: one source node per file, plus a planner that expands into the work.
    ///
    /// The planner is a node like any other, which is what makes the *plan itself* cacheable:
    /// next quarter's identical request against unchanged files does not re-plan (section 6).
    pub fn graph(&self, store: &Store) -> Result<TaskGraph, TaskError> {
        if self.files.is_empty() {
            return Err(TaskError::NoFiles);
        }
        let mut graph = TaskGraph::new();
        let mut paths: Vec<String> = Vec::new();

        for path in &self.files {
            let rel = path.to_string_lossy().replace('\\', "/");
            let full = match &self.root {
                Some(r) => r.join(path),
                None => path.clone(),
            };
            let bytes = std::fs::read(&full).map_err(|source| TaskError::Read {
                path: rel.clone(),
                source,
            })?;
            let digest = store.content.put(&bytes)?;
            paths.push(rel.clone());
            graph
                .add(
                    NodeSpec::new(nodes::key::source(&rel), NodeKind::Data).op(NodeOp::Source {
                        path: rel.clone(),
                        content: digest,
                    }
                    .encode()),
                )
                .expect("one source node per file");
            graph
                .add(
                    NodeSpec::new(nodes::key::outline(&rel), NodeKind::Context)
                        .op(NodeOp::Outline { path: rel.clone() }.encode())
                        .dep(nodes::key::source(&rel)),
                )
                .expect("one outline node per file");
        }

        // The planner depends on outlines, not on file contents. An edit that changes a function
        // body but no signature leaves every outline byte identical, so the plan is a cache hit
        // and the whole subgraph it emitted is replayed for free (section 6).
        let mut plan = NodeSpec::new(nodes::key::plan(), NodeKind::Plan)
            .op(NodeOp::Plan {
                instruction: self.instruction.clone(),
                paths: paths.clone(),
            }
            .encode())
            .env(Env::new(
                "planner",
                prompts::version(prompts::PLAN_SYSTEM),
                "orchestrator-v1",
            ));
        for rel in &paths {
            plan = plan.dep(nodes::key::outline(rel));
        }
        graph.add(plan).expect("one plan node");
        Ok(graph)
    }

    /// Run to completion against a store rooted at `store_root`.
    pub async fn run(
        self,
        store_root: impl AsRef<Path>,
        runtime: Arc<Runtime>,
    ) -> Result<RunReport, TaskError> {
        let store = Store::open(store_root)?;
        let graph = self.graph(&store)?;
        let engine = Engine::new(store, EngineConfig::default());
        let id = self
            .task_id
            .clone()
            .unwrap_or_else(|| format!("task-{}", Digest::of(self.instruction.as_bytes()).short()));
        Ok(engine.run(&id, &graph, runtime, self.events).await?)
    }

    /// Run against an engine the caller already has, so several tasks can share one daemon.
    pub async fn run_on(
        self,
        engine: &Engine,
        runtime: Arc<Runtime>,
    ) -> Result<RunReport, TaskError> {
        let graph = self.graph(engine.store())?;
        let id = self
            .task_id
            .clone()
            .unwrap_or_else(|| format!("task-{}", Digest::of(self.instruction.as_bytes()).short()));
        Ok(engine.run(&id, &graph, runtime, self.events).await?)
    }
}

/// Read back the final bytes a task produced for one file.
///
/// Nothing is written to disk by the runtime itself: a task produces content, and committing that
/// content to the working tree is a separate, explicit step. That is what makes a task safe to
/// run speculatively, cancel, or replay.
pub fn result_for<'a>(report: &'a RunReport, path: &str) -> Option<&'a Digest> {
    let wanted = format!("{path}@");
    report
        .nodes
        .iter()
        .filter(|n| {
            n.key.as_str().contains(&wanted) || n.key.as_str().contains(&format!("{path}#"))
        })
        .filter(|n| !n.outputs.is_empty())
        .max_by_key(|n| n.finished_at_ms)
        .and_then(|n| n.outputs.first())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flash_adapter_code::CodeAdapter;
    use model::ScriptedModel;
    use serde_json::json;

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[tokio::test]
    async fn a_task_graph_has_one_source_per_file_and_one_planner() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.rs", "fn a() {}\n");
        write(dir.path(), "b.rs", "fn b() {}\n");
        let store = Store::open(dir.path().join(".flash")).unwrap();

        let graph = Task::new("do something")
            .root(dir.path())
            .file("a.rs")
            .file("b.rs")
            .graph(&store)
            .unwrap();

        assert_eq!(graph.len(), 5, "two sources, two outlines, one plan");
        assert!(graph.get(&flash_core::NodeKey::new("plan")).is_some());
        assert!(
            graph
                .get(&flash_core::NodeKey::new("outline:a.rs"))
                .is_some()
        );
    }

    #[tokio::test]
    async fn a_task_with_no_files_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join(".flash")).unwrap();
        assert!(matches!(
            Task::new("x").graph(&store),
            Err(TaskError::NoFiles)
        ));
    }

    #[tokio::test]
    async fn a_missing_file_is_reported_with_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join(".flash")).unwrap();
        let err = Task::new("x")
            .root(dir.path())
            .file("nope.rs")
            .graph(&store)
            .unwrap_err();
        assert!(format!("{err}").contains("nope.rs"), "{err}");
    }

    #[tokio::test]
    async fn a_changed_file_moves_its_source_node_but_not_the_plan_op() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.rs", "fn a() {}\n");
        let store = Store::open(dir.path().join(".flash")).unwrap();
        let first = Task::new("x")
            .root(dir.path())
            .file("a.rs")
            .graph(&store)
            .unwrap();
        let src_before = first
            .get(&flash_core::NodeKey::new("src:a.rs"))
            .unwrap()
            .op
            .clone();
        let plan_before = first
            .get(&flash_core::NodeKey::new("plan"))
            .unwrap()
            .op
            .clone();

        write(dir.path(), "a.rs", "fn a() { 1 }\n");
        let second = Task::new("x")
            .root(dir.path())
            .file("a.rs")
            .graph(&store)
            .unwrap();
        let src_after = second
            .get(&flash_core::NodeKey::new("src:a.rs"))
            .unwrap()
            .op
            .clone();
        let plan_after = second
            .get(&flash_core::NodeKey::new("plan"))
            .unwrap()
            .op
            .clone();

        assert_ne!(
            src_before, src_after,
            "the source node carries the content hash, so it must move"
        );
        // The plan op names paths, not contents. Whether the planner re-runs is decided by the
        // outline node's *output*, which is where the body-versus-structure distinction lives.
        assert_eq!(plan_before, plan_after);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_body_edit_does_not_re_plan_but_a_new_entity_does() {
        // Section 6, in miniature: "the outline node hits unless the planner sees new headers".
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "lib.rs", "fn answer() -> u32 {\n    41\n}\n");

        let make = || {
            Runtime::new(
                vec![Arc::new(CodeAdapter::new())],
                Arc::new(ScriptedModel::new().otherwise(json!({
                    "edits": [{"path":"lib.rs","target":"fn:answer","instruction":"return 42"}]
                }))),
                Arc::new(ScriptedModel::new().otherwise(json!([
                    {"op":"replace_body","entity":"fn:answer","body":"42"}
                ]))),
                RuntimeConfig {
                    workspace: Some(dir.path().to_path_buf()),
                    ..Default::default()
                },
            )
        };

        let first = make();
        Task::new("make answer return 42")
            .root(dir.path())
            .file("lib.rs")
            .id("t1")
            .run(dir.path().join(".flash"), first)
            .await
            .unwrap();

        // A body change: same entities, so the outline is byte identical and the plan hits.
        write(dir.path(), "lib.rs", "fn answer() -> u32 {\n    40\n}\n");
        let second = make();
        let warm = Task::new("make answer return 42")
            .root(dir.path())
            .file("lib.rs")
            .id("t1")
            .run(dir.path().join(".flash"), second)
            .await
            .unwrap();
        assert_eq!(
            warm.node("plan").unwrap().status,
            flash_engine::NodeStatus::Hit,
            "a body edit must not re-plan"
        );

        // A new function: the outline moves, so the planner has to look again.
        write(
            dir.path(),
            "lib.rs",
            "fn answer() -> u32 {\n    40\n}\n\nfn extra() -> u32 {\n    1\n}\n",
        );
        let third = make();
        let structural = Task::new("make answer return 42")
            .root(dir.path())
            .file("lib.rs")
            .id("t1")
            .run(dir.path().join(".flash"), third)
            .await
            .unwrap();
        assert_eq!(
            structural.node("plan").unwrap().status,
            flash_engine::NodeStatus::Expanded,
            "a new entity must re-plan"
        );
    }

    /// The end to end path, with both models scripted: plan, pack, edit, verify.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_scripted_task_edits_a_real_file_through_the_whole_pipeline() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "lib.rs", "fn answer() -> u32 {\n    41\n}\n");

        let planner = ScriptedModel::new().otherwise(json!({
            "edits": [{
                "path": "lib.rs",
                "target": "fn:answer",
                "instruction": "return 42"
            }]
        }));
        let executor = ScriptedModel::new().otherwise(json!([
            {"op": "replace_body", "entity": "fn:answer", "body": "42"}
        ]));

        let runtime = Runtime::new(
            vec![Arc::new(CodeAdapter::new())],
            Arc::new(planner),
            Arc::new(executor),
            RuntimeConfig {
                workspace: Some(dir.path().to_path_buf()),
                ..Default::default()
            },
        );

        let report = Task::new("make answer return 42")
            .root(dir.path())
            .file("lib.rs")
            .id("t1")
            .run(dir.path().join(".flash"), runtime.clone())
            .await
            .unwrap();

        assert!(
            report.ok(),
            "task failed: {:?}",
            report
                .nodes
                .iter()
                .filter(|n| !n.diagnostics.is_empty())
                .map(|n| (n.key.clone(), n.diagnostics.clone()))
                .collect::<Vec<_>>()
        );

        let store = Store::open(dir.path().join(".flash")).unwrap();
        let digest = result_for(&report, "lib.rs").expect("a result for the edited file");
        let text = String::from_utf8(store.content.get(digest).unwrap()).unwrap();
        assert!(text.contains("42"), "{text}");
        assert!(
            text.contains("fn answer() -> u32"),
            "signature kept: {text}"
        );

        let m = runtime.metrics.snapshot();
        assert_eq!(m.ops_applied, 1);
        assert_eq!(m.ops_rejected, 0);
        assert_eq!(m.op_resolve_rate(), 1.0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rerunning_an_unchanged_task_calls_no_model_at_all() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "lib.rs", "fn answer() -> u32 {\n    41\n}\n");

        let make_runtime = || {
            Runtime::new(
                vec![Arc::new(CodeAdapter::new())],
                Arc::new(ScriptedModel::new().otherwise(json!({
                    "edits": [{"path":"lib.rs","target":"fn:answer","instruction":"return 42"}]
                }))),
                Arc::new(ScriptedModel::new().otherwise(json!([
                    {"op":"replace_body","entity":"fn:answer","body":"42"}
                ]))),
                RuntimeConfig {
                    workspace: Some(dir.path().to_path_buf()),
                    ..Default::default()
                },
            )
        };

        let first = make_runtime();
        let r1 = Task::new("make answer return 42")
            .root(dir.path())
            .file("lib.rs")
            .id("t1")
            .run(dir.path().join(".flash"), first.clone())
            .await
            .unwrap();
        assert!(r1.ok());
        assert!(first.metrics.snapshot().edits_attempted > 0);

        let second = make_runtime();
        let r2 = Task::new("make answer return 42")
            .root(dir.path())
            .file("lib.rs")
            .id("t1")
            .run(dir.path().join(".flash"), second.clone())
            .await
            .unwrap();

        assert!(r2.is_hot(), "an unchanged task must be hot");
        assert_eq!(
            second.metrics.snapshot().edits_attempted,
            0,
            "a hot task must not call the executor model"
        );
    }
}

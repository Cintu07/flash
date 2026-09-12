//! Shared test scaffolding.

use flash_engine::{Engine, EngineConfig, Events, NodeExecutor, RunReport, TaskGraph};
use flash_store::Store;
use std::path::Path;
use std::sync::Arc;

pub fn engine_at(dir: &Path) -> Arc<Engine> {
    let store = Store::open(dir).expect("store opens");
    Engine::new(store, EngineConfig::default())
}

pub async fn run(
    engine: &Engine,
    task: &str,
    graph: &TaskGraph,
    exec: Arc<dyn NodeExecutor>,
) -> RunReport {
    engine
        .run(task, graph, exec, Events::none())
        .await
        .expect("run completes")
}

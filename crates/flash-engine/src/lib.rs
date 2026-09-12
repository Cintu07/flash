//! flash-engine: the incremental build graph that every task runs on.
//!
//! The engine knows nothing about code, documents, slides or sheets. It knows nodes, content
//! hashes, a memo store and a clock. Adapters supply meaning; this crate supplies the property
//! that repeat work costs nothing and new work is the only work.
//!
//! ```no_run
//! use flash_engine::{Engine, EngineConfig, Events, NodeSpec, TaskGraph};
//! use flash_core::{Digest, NodeKind};
//! use flash_store::Store;
//! use std::sync::Arc;
//!
//! # async fn demo(executor: Arc<dyn flash_engine::NodeExecutor>) -> Result<(), Box<dyn std::error::Error>> {
//! let graph = TaskGraph::new()
//!     .with(NodeSpec::source("csv:q3", Digest::of(b"...")))
//!     .with(NodeSpec::new("section:methodology", NodeKind::Op).dep("csv:q3"));
//!
//! let engine = Engine::new(Store::open(".flash")?, EngineConfig::default());
//! let report = engine.run("report-q3", &graph, executor, Events::none()).await?;
//! println!("{} computed, {} hit", report.computed(), report.hits());
//! # Ok(()) }
//! ```

pub mod eta;
pub mod exec;
pub mod graph;
pub mod report;
pub mod scheduler;

pub use eta::{Eta, NodeCost};
pub use exec::{
    ExecCtx, ExecFuture, Expansion, JobHandle, JobId, JobRegistry, NodeExecutor, NodeResult,
    Progress,
};
pub use graph::{GraphError, NodeSpec, Plan, TaskGraph};
pub use report::{Event, Events, NodeReport, NodeStatus, RunReport};
pub use scheduler::{Engine, EngineConfig, EngineError, run_task};

/// Re-exported so downstream crates need one dependency, not three.
pub use flash_core as core_types;
pub use flash_store as store;

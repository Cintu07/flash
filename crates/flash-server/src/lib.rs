//! flash-server: the runtime as a daemon (d8, d9).
//!
//! One json-rpc 2.0 surface, line delimited, spoken over stdio. Two dialects sit on it:
//!
//! * **native** (`flash/run`, `flash/status`, `flash/cancel`, `flash/jobs`) - what the thin cli
//!   and an acp client use, including reattaching to a running task by id;
//! * **mcp** (`initialize`, `tools/list`, `tools/call`) - so any mcp client can drive the runtime
//!   without knowing anything about it.
//!
//! No editor, no ide (d9). The daemon owns the engine, the store and the job registry, so closing
//! a client does not stop a task: jobs keep running and the client reattaches by task id, which is
//! the property section 3.2 asks for and the reason the server holds state at all.

use flash_adapter::Adapter;
use flash_adapter_code::CodeAdapter;
use flash_adapter_doc::DocAdapter;
use flash_adapter_sheets::SheetsAdapter;
use flash_adapter_slides::SlidesAdapter;
use flash_engine::{Engine, EngineConfig, Events, RunReport};
use flash_orchestrator::model::{CommandModel, ModelClient, ScriptedModel};
use flash_orchestrator::{Runtime, RuntimeConfig, Task};
use flash_store::Store;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub const MCP_PROTOCOL_VERSION: &str = "2025-03-26";

/// What a task looks like to a client that comes back later.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum TaskState {
    Running,
    Done,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskStatus {
    pub id: String,
    pub state: TaskState,
    pub instruction: String,
    /// Present once the task has finished.
    pub summary: Option<TaskSummary>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskSummary {
    pub wall_ms: u64,
    pub nodes: usize,
    pub computed: usize,
    pub hits: usize,
    pub saved_ms: u64,
    pub first_change_ms: Option<u64>,
    pub green: bool,
    pub diagnostics: Vec<String>,
    /// path -> content hash of the result, for a client that wants to fetch or commit it.
    pub outputs: HashMap<String, String>,
}

impl TaskSummary {
    fn from_report(report: &RunReport, paths: &[String]) -> TaskSummary {
        let mut outputs = HashMap::new();
        for p in paths {
            if let Some(d) = flash_orchestrator::result_for(report, p) {
                outputs.insert(p.clone(), d.hex());
            }
        }
        TaskSummary {
            wall_ms: report.wall_ms,
            nodes: report.nodes.len(),
            computed: report.computed(),
            hits: report.hits(),
            saved_ms: report.saved_ms,
            first_change_ms: report.first_change_ms,
            green: report.ok(),
            diagnostics: report
                .nodes
                .iter()
                .flat_map(|n| n.diagnostics.clone())
                .take(flash_adapter::MAX_DIAGNOSTICS)
                .collect(),
            outputs,
        }
    }
}

pub struct ServerConfig {
    pub store_root: PathBuf,
    pub workspace: PathBuf,
    /// The command that speaks to a model, if one is configured. Without it the server answers
    /// with a clear error instead of pretending to work.
    pub planner_cmd: Option<Vec<String>>,
    pub executor_cmd: Option<Vec<String>>,
}

impl ServerConfig {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        let workspace = workspace.into();
        ServerConfig {
            store_root: workspace.join(".flash"),
            workspace,
            planner_cmd: None,
            executor_cmd: None,
        }
    }
}

/// The planner and executor, in that order (d3: two models, one protocol).
type ModelPair = (Arc<dyn ModelClient>, Arc<dyn ModelClient>);

pub struct Server {
    engine: Arc<Engine>,
    cfg: ServerConfig,
    tasks: Mutex<HashMap<String, TaskStatus>>,
    /// Overridable for tests: a scripted pair instead of spawned processes.
    models: Mutex<Option<ModelPair>>,
}

impl Server {
    pub fn new(cfg: ServerConfig) -> std::io::Result<Arc<Server>> {
        let store = Store::open(&cfg.store_root).map_err(std::io::Error::other)?;
        Ok(Arc::new(Server {
            engine: Engine::new(store, EngineConfig::default()),
            cfg,
            tasks: Mutex::new(HashMap::new()),
            models: Mutex::new(None),
        }))
    }

    /// Point the server at scripted models. Used by tests and by `flash demo`.
    pub fn with_models(
        self: &Arc<Self>,
        planner: Arc<dyn ModelClient>,
        executor: Arc<dyn ModelClient>,
    ) {
        *self.models.lock().unwrap() = Some((planner, executor));
    }

    pub fn engine(&self) -> &Arc<Engine> {
        &self.engine
    }

    fn adapters(&self) -> Vec<Arc<dyn Adapter>> {
        vec![
            Arc::new(CodeAdapter::new()),
            Arc::new(DocAdapter::new()),
            Arc::new(SheetsAdapter::new()),
            Arc::new(SlidesAdapter::new()),
        ]
    }

    fn runtime(&self) -> Result<Arc<Runtime>, String> {
        let (planner, executor) = match self.models.lock().unwrap().clone() {
            Some(pair) => pair,
            None => {
                let planner = self.cfg.planner_cmd.as_ref().ok_or(
                    "no planner model configured: pass --planner-cmd or use scripted models",
                )?;
                let executor = self
                    .cfg
                    .executor_cmd
                    .as_ref()
                    .ok_or("no executor model configured: pass --executor-cmd")?;
                let to_client = |cmd: &Vec<String>| -> Arc<dyn ModelClient> {
                    let args: Vec<&str> = cmd[1..].iter().map(|s| s.as_str()).collect();
                    Arc::new(CommandModel::new(cmd[0].clone(), &args))
                };
                (to_client(planner), to_client(executor))
            }
        };

        Ok(Runtime::new(
            self.adapters(),
            planner,
            executor,
            RuntimeConfig {
                workspace: Some(self.cfg.workspace.clone()),
                ..Default::default()
            },
        ))
    }

    /// Run a task to completion. The daemon owns it, so a client that disconnects loses nothing.
    pub async fn run_task(
        self: &Arc<Self>,
        id: &str,
        instruction: &str,
        files: &[String],
    ) -> TaskStatus {
        self.tasks.lock().unwrap().insert(
            id.to_string(),
            TaskStatus {
                id: id.to_string(),
                state: TaskState::Running,
                instruction: instruction.to_string(),
                summary: None,
                error: None,
            },
        );

        let status = match self.runtime() {
            Err(e) => TaskStatus {
                id: id.to_string(),
                state: TaskState::Failed,
                instruction: instruction.to_string(),
                summary: None,
                error: Some(e),
            },
            Ok(runtime) => {
                let result = Task::new(instruction)
                    .root(&self.cfg.workspace)
                    .files(files.to_vec())
                    .id(id)
                    .events(Events::none())
                    .run_on(&self.engine, runtime)
                    .await;
                match result {
                    Ok(report) => {
                        let summary = TaskSummary::from_report(&report, files);
                        TaskStatus {
                            id: id.to_string(),
                            state: if summary.green {
                                TaskState::Done
                            } else {
                                TaskState::Failed
                            },
                            instruction: instruction.to_string(),
                            summary: Some(summary),
                            error: None,
                        }
                    }
                    Err(e) => TaskStatus {
                        id: id.to_string(),
                        state: TaskState::Failed,
                        instruction: instruction.to_string(),
                        summary: None,
                        error: Some(e.to_string()),
                    },
                }
            }
        };

        self.tasks
            .lock()
            .unwrap()
            .insert(id.to_string(), status.clone());
        status
    }

    pub fn status(&self, id: &str) -> Option<TaskStatus> {
        self.tasks.lock().unwrap().get(id).cloned()
    }

    pub fn content(&self, hex: &str) -> Option<String> {
        let d = flash_core::Digest::from_hex(hex)?;
        self.engine
            .store()
            .content
            .get(&d)
            .ok()
            .map(|b| String::from_utf8_lossy(&b).to_string())
    }

    /// Handle one json-rpc request. Returns None for a notification.
    pub async fn handle(self: &Arc<Self>, line: &str) -> Option<String> {
        // Windows clients routinely prefix the first line with a utf-8 byte order mark. Failing
        // the handshake over three invisible bytes is a miserable first five minutes.
        let line = line.trim_start_matches('\u{feff}');
        let req: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                return Some(error_response(
                    Value::Null,
                    -32700,
                    &format!("parse error: {e}"),
                ));
            }
        };

        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = req.get("params").cloned().unwrap_or(json!({}));
        let is_notification = req.get("id").is_none();

        let result: Result<Value, (i64, String)> = match method {
            // ---- mcp ---------------------------------------------------------------------
            "initialize" => Ok(json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "flash", "version": env!("CARGO_PKG_VERSION") }
            })),
            "notifications/initialized" | "initialized" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tool_definitions() })),
            "tools/call" => {
                let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                match name {
                    "flash_run" => match self.run_from_params(&args).await {
                        Ok(status) => Ok(json!({
                            "content": [{ "type": "text", "text": render_status(&status) }],
                            "isError": status.state == TaskState::Failed
                        })),
                        Err(e) => Ok(json!({
                            "content": [{ "type": "text", "text": e }],
                            "isError": true
                        })),
                    },
                    "flash_status" => {
                        let id = args.get("task_id").and_then(|v| v.as_str()).unwrap_or("");
                        let text = match self.status(id) {
                            Some(s) => render_status(&s),
                            None => format!("no task called {id}"),
                        };
                        Ok(json!({ "content": [{ "type": "text", "text": text }] }))
                    }
                    other => Err((-32602, format!("no tool called {other}"))),
                }
            }

            // ---- native ------------------------------------------------------------------
            "flash/run" => match self.run_from_params(&params).await {
                Ok(status) => serde_json::to_value(status).map_err(|e| (-32603, e.to_string())),
                Err(e) => Err((-32602, e)),
            },
            "flash/status" => {
                let id = params.get("task_id").and_then(|v| v.as_str()).unwrap_or("");
                match self.status(id) {
                    Some(s) => serde_json::to_value(s).map_err(|e| (-32603, e.to_string())),
                    None => Err((-32602, format!("no task called {id}"))),
                }
            }
            "flash/jobs" => Ok(json!({
                "jobs": self
                    .engine
                    .jobs()
                    .live()
                    .into_iter()
                    .map(|(jid, key)| json!({ "job": jid.0, "node": key.as_str() }))
                    .collect::<Vec<_>>()
            })),
            "flash/cancel" => {
                let job = params.get("job").and_then(|v| v.as_u64()).unwrap_or(0);
                let cancelled = self.engine.jobs().cancel(flash_engine::JobId(job));
                Ok(json!({ "cancelled": cancelled }))
            }
            "flash/content" => {
                let hex = params.get("hash").and_then(|v| v.as_str()).unwrap_or("");
                match self.content(hex) {
                    Some(text) => Ok(json!({ "text": text })),
                    None => Err((-32602, format!("no content for {hex}"))),
                }
            }
            "shutdown" => Ok(json!({ "ok": true })),
            other => Err((-32601, format!("unknown method {other}"))),
        };

        if is_notification {
            return None;
        }
        Some(match result {
            Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }).to_string(),
            Err((code, message)) => error_response(id, code, &message),
        })
    }

    async fn run_from_params(self: &Arc<Self>, params: &Value) -> Result<TaskStatus, String> {
        let instruction = params
            .get("instruction")
            .and_then(|v| v.as_str())
            .ok_or("instruction is required")?;
        let files: Vec<String> = params
            .get("files")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if files.is_empty() {
            return Err("at least one file is required".into());
        }
        let id = params
            .get("task_id")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| {
                format!(
                    "task-{}",
                    flash_core::Digest::of(instruction.as_bytes()).short()
                )
            });
        Ok(self.run_task(&id, instruction, &files).await)
    }

    /// Read json-rpc lines from stdin, write responses to stdout, until the client hangs up.
    pub async fn serve_stdio(self: &Arc<Self>) -> std::io::Result<()> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let stdin = BufReader::new(tokio::io::stdin());
        let mut lines = stdin.lines();
        let mut stdout = tokio::io::stdout();
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            if let Some(response) = self.handle(&line).await {
                stdout.write_all(response.as_bytes()).await?;
                stdout.write_all(b"\n").await?;
                stdout.flush().await?;
            }
        }
        Ok(())
    }
}

fn error_response(id: Value, code: i64, message: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
    .to_string()
}

fn render_status(s: &TaskStatus) -> String {
    match &s.summary {
        None => format!(
            "{} [{}]{}",
            s.id,
            match s.state {
                TaskState::Running => "running",
                TaskState::Done => "done",
                TaskState::Failed => "failed",
            },
            s.error
                .as_ref()
                .map(|e| format!(": {e}"))
                .unwrap_or_default()
        ),
        Some(sum) => {
            let mut text = format!(
                "{} [{}] {} ms, {} computed, {} hit, {} ms saved",
                s.id,
                if sum.green { "green" } else { "red" },
                sum.wall_ms,
                sum.computed,
                sum.hits,
                sum.saved_ms
            );
            for (path, hash) in &sum.outputs {
                text.push_str(&format!("\n  {path} -> {}", &hash[..12.min(hash.len())]));
            }
            for d in &sum.diagnostics {
                text.push_str(&format!("\n  ! {d}"));
            }
            text
        }
    }
}

fn tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "flash_run",
            "description": "Run an incremental task over one or more files. Repeat work is served \
                            from the cache, so re-running an unchanged task costs nothing.",
            "inputSchema": {
                "type": "object",
                "required": ["instruction", "files"],
                "properties": {
                    "instruction": { "type": "string" },
                    "files": { "type": "array", "items": { "type": "string" } },
                    "task_id": { "type": "string", "description": "reuse to resume or re-run" }
                }
            }
        }),
        json!({
            "name": "flash_status",
            "description": "Status of a task by id, including results and diagnostics.",
            "inputSchema": {
                "type": "object",
                "required": ["task_id"],
                "properties": { "task_id": { "type": "string" } }
            }
        }),
    ]
}

/// Convenience for the cli: a server wired to scripted models, for demos and tests.
pub fn scripted_server(workspace: &Path, plan: Value, ops: Value) -> std::io::Result<Arc<Server>> {
    let server = Server::new(ServerConfig::new(workspace))?;
    server.with_models(
        Arc::new(ScriptedModel::new().otherwise(plan)),
        Arc::new(ScriptedModel::new().otherwise(ops)),
    );
    Ok(server)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> (tempfile::TempDir, Arc<Server>) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("lib.rs"),
            "fn answer() -> u32 {\n    41\n}\n",
        )
        .unwrap();
        let server = scripted_server(
            dir.path(),
            json!({"edits":[{"path":"lib.rs","target":"fn:answer","instruction":"return 42"}]}),
            json!([{"op":"replace_body","entity":"fn:answer","body":"42"}]),
        )
        .unwrap();
        (dir, server)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn mcp_initialize_advertises_tools() {
        let (_d, server) = workspace();
        let response = server
            .handle(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["result"]["protocolVersion"], MCP_PROTOCOL_VERSION);
        assert!(v["result"]["capabilities"]["tools"].is_object());
        assert_eq!(v["result"]["serverInfo"]["name"], "flash");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tools_list_describes_what_a_client_can_call() {
        let (_d, server) = workspace();
        let response = server
            .handle(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#)
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&response).unwrap();
        let names: Vec<&str> = v["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"flash_run"), "{names:?}");
        assert!(names.contains(&"flash_status"), "{names:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_task_runs_over_mcp_and_reports_its_result() {
        let (_d, server) = workspace();
        let call = json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"flash_run","arguments":{
                "instruction":"make answer return 42","files":["lib.rs"],"task_id":"t1"}}
        });
        let response = server.handle(&call.to_string()).await.unwrap();
        let v: Value = serde_json::from_str(&response).unwrap();
        let text = v["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("green"), "{text}");
        assert!(text.contains("lib.rs ->"), "{text}");
        assert_ne!(v["result"]["isError"], json!(true));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_client_can_come_back_later_and_ask_by_task_id() {
        let (_d, server) = workspace();
        server
            .run_task("t2", "make answer return 42", &["lib.rs".to_string()])
            .await;

        // A brand new "connection" asking about a task it never started.
        let response = server
            .handle(r#"{"jsonrpc":"2.0","id":4,"method":"flash/status","params":{"task_id":"t2"}}"#)
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["result"]["state"], "done");
        assert!(v["result"]["summary"]["green"].as_bool().unwrap());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_result_content_can_be_fetched_by_hash() {
        let (_d, server) = workspace();
        let status = server
            .run_task("t3", "make answer return 42", &["lib.rs".to_string()])
            .await;
        let hash = status.summary.unwrap().outputs["lib.rs"].clone();
        let response = server
            .handle(
                &json!({"jsonrpc":"2.0","id":5,"method":"flash/content","params":{"hash":hash}})
                    .to_string(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&response).unwrap();
        assert!(v["result"]["text"].as_str().unwrap().contains("42"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unknown_method_is_a_json_rpc_error_not_a_crash() {
        let (_d, server) = workspace();
        let response = server
            .handle(r#"{"jsonrpc":"2.0","id":6,"method":"nope"}"#)
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["error"]["code"], -32601);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_byte_order_mark_does_not_break_the_handshake() {
        let (_d, server) = workspace();
        let response = server
            .handle(
                "\u{feff}{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}",
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["result"]["protocolVersion"], MCP_PROTOCOL_VERSION);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn malformed_json_gets_a_parse_error_response() {
        let (_d, server) = workspace();
        let response = server.handle("{not json").await.unwrap();
        let v: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["error"]["code"], -32700);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_notification_gets_no_response() {
        let (_d, server) = workspace();
        assert!(
            server
                .handle(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                .await
                .is_none()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn running_without_a_model_configured_says_so_rather_than_failing_obscurely() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
        let server = Server::new(ServerConfig::new(dir.path())).unwrap();
        let status = server
            .run_task("t", "do something", &["a.rs".to_string()])
            .await;
        assert_eq!(status.state, TaskState::Failed);
        assert!(
            status
                .error
                .unwrap()
                .contains("no planner model configured"),
            "the error must name the missing piece"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_run_of_the_same_task_is_served_from_the_cache() {
        let (_d, server) = workspace();
        let first = server
            .run_task("t4", "make answer return 42", &["lib.rs".to_string()])
            .await;
        let second = server
            .run_task("t4", "make answer return 42", &["lib.rs".to_string()])
            .await;
        assert!(first.summary.unwrap().computed > 0);
        let s = second.summary.unwrap();
        assert_eq!(s.computed, 0, "the daemon keeps its store between tasks");
        assert!(s.saved_ms > 0 || s.hits > 0);
    }
}

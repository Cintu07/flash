//! Model clients (d3: two models, one protocol).
//!
//! The runtime never talks to a vendor sdk. It talks to [`ModelClient`], whose whole contract is
//! "here is a prompt and a json schema, give me json back". Any planner/executor pair that
//! supports schema-constrained output satisfies it, which is exactly what d3 asks for and is why
//! there is no training anywhere in this plan.
//!
//! Four implementations ship here, and the reason for each is different:
//!
//! * [`ScriptedModel`] - fixtures. Makes the whole pipeline testable with no network and no
//!   nondeterminism, which matters because caching a sampled answer is the one thing this design
//!   must get right.
//! * [`ReplayModel`] - a cassette. Records real calls once, replays them forever. This is what
//!   makes the benchmark reproducible by one command on a fresh box (section 7) without paying
//!   for tokens on every rerun, and without the results drifting as a provider updates a model.
//! * [`CommandModel`] - spawns a process, writes json to stdin, reads json from stdout. Any cli
//!   that can be pointed at a model works, so the runtime does not pick a vendor.
//! * An http client is the obvious fourth. It is deliberately not here: it would be the only
//!   dependency in the workspace that needs tls, and it adds nothing this trait does not already
//!   express. `CommandModel` covers real use today.

use flash_core::{Digest, Hasher};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("model call failed: {0}")]
    Call(String),
    #[error("model returned something that is not valid json: {0}")]
    NotJson(String),
    #[error("no recorded response for this request; run with recording enabled first")]
    NoRecording,
}

pub type ModelResult<T> = std::result::Result<T, ModelError>;

/// One call. Everything in here is hashed into the node's action key, so two calls that differ in
/// any field are different nodes, and two that match are one cached answer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelRequest {
    /// Exact version string. d11: a new model version is a new table, never a silent change.
    pub model_id: String,
    /// Hash of the prompt template, so bumping one prompt invalidates only its nodes (d4).
    pub prompt_version: String,
    pub system: String,
    pub user: String,
    /// The schema the response must satisfy. None for free text, which nothing here uses.
    pub schema: Option<serde_json::Value>,
    pub max_output_tokens: u32,
}

impl ModelRequest {
    /// Content identity of a call. The replay cassette is keyed on this.
    pub fn digest(&self) -> Digest {
        let mut h = Hasher::new("flash.model.request.v1");
        h.str(&self.model_id);
        h.str(&self.prompt_version);
        h.str(&self.system);
        h.str(&self.user);
        h.str(&serde_json::to_string(&self.schema).unwrap_or_default());
        h.u64(self.max_output_tokens as u64);
        h.finish()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelResponse {
    /// Parsed json when a schema was requested.
    pub json: Option<serde_json::Value>,
    pub text: String,
    pub input_tokens: u32,
    pub output_tokens: u32,
}

impl ModelResponse {
    pub fn from_json(v: serde_json::Value) -> Self {
        let text = v.to_string();
        ModelResponse {
            input_tokens: 0,
            output_tokens: (text.len() / 4) as u32,
            json: Some(v),
            text,
        }
    }
}

pub type ModelFuture<'a> = Pin<Box<dyn Future<Output = ModelResult<ModelResponse>> + Send + 'a>>;

pub trait ModelClient: Send + Sync + 'static {
    fn call<'a>(&'a self, req: &'a ModelRequest) -> ModelFuture<'a>;

    /// How many tokens per second this client decodes at, when known. Used only to seed job
    /// classification before there is any history.
    fn tps_hint(&self) -> u32 {
        100
    }
}

/// Counters the benchmark publishes (d11).
#[derive(Default, Debug)]
pub struct ModelMetrics {
    pub calls: AtomicU64,
    pub input_tokens: AtomicU64,
    pub output_tokens: AtomicU64,
}

impl ModelMetrics {
    pub fn observe(&self, r: &ModelResponse) {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.input_tokens
            .fetch_add(r.input_tokens as u64, Ordering::Relaxed);
        self.output_tokens
            .fetch_add(r.output_tokens as u64, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> (u64, u64, u64) {
        (
            self.calls.load(Ordering::Relaxed),
            self.input_tokens.load(Ordering::Relaxed),
            self.output_tokens.load(Ordering::Relaxed),
        )
    }
}

/// Fixtures, keyed by a substring of the prompt. Deterministic and offline.
pub struct ScriptedModel {
    /// (marker that must appear in the user prompt, response)
    rules: Vec<(String, serde_json::Value)>,
    default: Option<serde_json::Value>,
    pub metrics: ModelMetrics,
    seen: Mutex<Vec<String>>,
}

impl ScriptedModel {
    pub fn new() -> Self {
        ScriptedModel {
            rules: Vec::new(),
            default: None,
            metrics: ModelMetrics::default(),
            seen: Mutex::new(Vec::new()),
        }
    }

    /// Respond with `response` to any prompt containing `marker`, in either the system or the
    /// user message. First match wins, so more specific rules go first.
    ///
    /// Matching the system message matters: the escalation ladder changes *systems* between
    /// attempts (edit, repair, diff), and a fixture that could only see the user message could
    /// not tell the runtime's second attempt from its third.
    pub fn on(mut self, marker: impl Into<String>, response: serde_json::Value) -> Self {
        self.rules.push((marker.into(), response));
        self
    }

    pub fn otherwise(mut self, response: serde_json::Value) -> Self {
        self.default = Some(response);
        self
    }

    /// Prompts this model was asked, for tests that assert on what was and was not called.
    pub fn prompts(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }
}

impl Default for ScriptedModel {
    fn default() -> Self {
        ScriptedModel::new()
    }
}

impl ModelClient for ScriptedModel {
    fn call<'a>(&'a self, req: &'a ModelRequest) -> ModelFuture<'a> {
        Box::pin(async move {
            self.seen.lock().unwrap().push(req.user.clone());
            let found = self
                .rules
                .iter()
                .find(|(marker, _)| {
                    req.user.contains(marker.as_str()) || req.system.contains(marker.as_str())
                })
                .map(|(_, v)| v.clone())
                .or_else(|| self.default.clone());
            match found {
                Some(v) => {
                    let resp = ModelResponse::from_json(v);
                    self.metrics.observe(&resp);
                    Ok(resp)
                }
                None => Err(ModelError::Call(format!(
                    "no scripted rule matches this prompt:\n{}",
                    req.user.chars().take(400).collect::<String>()
                ))),
            }
        })
    }

    fn tps_hint(&self) -> u32 {
        10_000
    }
}

/// Record once, replay forever.
///
/// The point is reproducibility, not speed: a published benchmark whose numbers move because a
/// provider retuned a model is not a benchmark. With a cassette, anyone can re-derive the tables
/// from the repo, and a rerun against a *new* model is an explicit, separate recording.
pub struct ReplayModel {
    inner: Option<Box<dyn ModelClient>>,
    cassette: std::path::PathBuf,
    memory: Mutex<HashMap<String, ModelResponse>>,
    pub metrics: ModelMetrics,
}

impl ReplayModel {
    /// Replay only. A request with no recording is an error rather than a live call.
    pub fn replay_only(cassette: impl Into<std::path::PathBuf>) -> std::io::Result<Self> {
        let cassette = cassette.into();
        let memory = load_cassette(&cassette)?;
        Ok(ReplayModel {
            inner: None,
            cassette,
            memory: Mutex::new(memory),
            metrics: ModelMetrics::default(),
        })
    }

    /// Replay what is recorded, call `inner` for anything new, and append it.
    pub fn recording(
        cassette: impl Into<std::path::PathBuf>,
        inner: Box<dyn ModelClient>,
    ) -> std::io::Result<Self> {
        let cassette = cassette.into();
        let memory = load_cassette(&cassette)?;
        Ok(ReplayModel {
            inner: Some(inner),
            cassette,
            memory: Mutex::new(memory),
            metrics: ModelMetrics::default(),
        })
    }

    fn persist(&self) -> std::io::Result<()> {
        let memory = self.memory.lock().unwrap();
        let json = serde_json::to_vec_pretty(&*memory)?;
        if let Some(parent) = self.cassette.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.cassette, json)
    }

    pub fn len(&self) -> usize {
        self.memory.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn load_cassette(path: &std::path::Path) -> std::io::Result<HashMap<String, ModelResponse>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).unwrap_or_default()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
        Err(e) => Err(e),
    }
}

impl ModelClient for ReplayModel {
    fn call<'a>(&'a self, req: &'a ModelRequest) -> ModelFuture<'a> {
        Box::pin(async move {
            let key = req.digest().hex();
            if let Some(hit) = self.memory.lock().unwrap().get(&key).cloned() {
                self.metrics.observe(&hit);
                return Ok(hit);
            }
            let Some(inner) = &self.inner else {
                return Err(ModelError::NoRecording);
            };
            let resp = inner.call(req).await?;
            self.memory.lock().unwrap().insert(key, resp.clone());
            self.persist()
                .map_err(|e| ModelError::Call(format!("could not write cassette: {e}")))?;
            self.metrics.observe(&resp);
            Ok(resp)
        })
    }
}

/// Talk to whatever cli the user already has.
///
/// The process receives the request as json on stdin and must print a json object on stdout with
/// at least a `text` field, or the response json itself. Deliberately unopinionated: this is how
/// the runtime supports a provider it has never heard of without a plugin system.
pub struct CommandModel {
    pub program: String,
    pub args: Vec<String>,
    pub metrics: ModelMetrics,
}

impl CommandModel {
    pub fn new(program: impl Into<String>, args: &[&str]) -> Self {
        CommandModel {
            program: program.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            metrics: ModelMetrics::default(),
        }
    }
}

impl ModelClient for CommandModel {
    fn call<'a>(&'a self, req: &'a ModelRequest) -> ModelFuture<'a> {
        Box::pin(async move {
            let payload = serde_json::to_vec(req).map_err(|e| ModelError::Call(e.to_string()))?;
            let program = self.program.clone();
            let args = self.args.clone();

            // Blocking process io on a worker thread: the scheduler must not stall behind it.
            let out =
                tokio::task::spawn_blocking(move || -> std::io::Result<std::process::Output> {
                    use std::io::Write;
                    use std::process::{Command, Stdio};
                    let mut child = Command::new(&program)
                        .args(&args)
                        .stdin(Stdio::piped())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .spawn()?;
                    child
                        .stdin
                        .as_mut()
                        .expect("stdin was piped")
                        .write_all(&payload)?;
                    child.wait_with_output()
                })
                .await
                .map_err(|e| ModelError::Call(e.to_string()))?
                .map_err(|e| ModelError::Call(format!("{} could not run: {e}", self.program)))?;

            if !out.status.success() {
                let err = String::from_utf8_lossy(&out.stderr);
                return Err(ModelError::Call(format!(
                    "{} exited with {}: {}",
                    self.program,
                    out.status,
                    err.lines().next().unwrap_or("")
                )));
            }

            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            let value: serde_json::Value = serde_json::from_str(stdout.trim())
                .map_err(|_| ModelError::NotJson(stdout.chars().take(200).collect()))?;

            let resp = match value.get("text").and_then(|t| t.as_str()) {
                // The cli wrapped the answer; the answer itself may still be json.
                Some(text) => ModelResponse {
                    json: serde_json::from_str(text).ok(),
                    text: text.to_string(),
                    input_tokens: value
                        .get("input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as u32,
                    output_tokens: value
                        .get("output_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or((text.len() / 4) as u64)
                        as u32,
                },
                None => ModelResponse::from_json(value),
            };
            self.metrics.observe(&resp);
            Ok(resp)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn req(user: &str) -> ModelRequest {
        ModelRequest {
            model_id: "test-exec-1".into(),
            prompt_version: "p1".into(),
            system: "you emit ops".into(),
            user: user.into(),
            schema: Some(json!({"type":"array"})),
            max_output_tokens: 512,
        }
    }

    #[tokio::test]
    async fn scripted_matches_the_most_specific_rule_first() {
        let m = ScriptedModel::new()
            .on(
                "fn:parse",
                json!([{"op":"replace_body","entity":"fn:parse","body":"1"}]),
            )
            .otherwise(json!([]));
        let r = m.call(&req("edit fn:parse please")).await.unwrap();
        assert_eq!(r.json.unwrap()[0]["entity"], "fn:parse");

        let fallback = m.call(&req("something else")).await.unwrap();
        assert_eq!(fallback.json.unwrap(), json!([]));
    }

    #[tokio::test]
    async fn an_unmatched_prompt_is_an_error_not_an_empty_answer() {
        let m = ScriptedModel::new().on("never", json!([]));
        assert!(m.call(&req("anything")).await.is_err());
    }

    #[test]
    fn request_identity_covers_every_field_that_can_change_an_answer() {
        let base = req("x");
        let mut other = base.clone();
        other.prompt_version = "p2".into();
        assert_ne!(base.digest(), other.digest());

        let mut model_changed = base.clone();
        model_changed.model_id = "test-exec-2".into();
        assert_ne!(base.digest(), model_changed.digest());

        assert_eq!(base.digest(), req("x").digest(), "identity must be stable");
    }

    #[tokio::test]
    async fn replay_serves_a_recorded_call_without_touching_the_inner_model() {
        let dir = tempfile::tempdir().unwrap();
        let cassette = dir.path().join("calls.json");

        let recorder = ReplayModel::recording(
            &cassette,
            Box::new(ScriptedModel::new().otherwise(json!({"answer": 1}))),
        )
        .unwrap();
        let first = recorder.call(&req("hello")).await.unwrap();
        assert_eq!(first.json.clone().unwrap()["answer"], 1);
        assert_eq!(recorder.len(), 1);
        drop(recorder);

        // A fresh replay-only client, no inner model at all.
        let replay = ReplayModel::replay_only(&cassette).unwrap();
        let again = replay.call(&req("hello")).await.unwrap();
        assert_eq!(again.json.unwrap()["answer"], 1);
    }

    #[tokio::test]
    async fn replay_only_refuses_an_unrecorded_call_rather_than_inventing_one() {
        let dir = tempfile::tempdir().unwrap();
        let replay = ReplayModel::replay_only(dir.path().join("empty.json")).unwrap();
        assert!(matches!(
            replay.call(&req("never recorded")).await,
            Err(ModelError::NoRecording)
        ));
    }

    #[tokio::test]
    async fn a_missing_command_is_reported_not_swallowed() {
        let m = CommandModel::new("definitely-not-a-real-binary-xyz", &[]);
        let err = m.call(&req("x")).await.unwrap_err();
        assert!(format!("{err}").contains("could not run"), "{err}");
    }

    #[tokio::test]
    async fn metrics_count_calls_and_tokens() {
        let m = ScriptedModel::new().otherwise(json!({"ok": true}));
        m.call(&req("a")).await.unwrap();
        m.call(&req("b")).await.unwrap();
        let (calls, _, out) = m.metrics.snapshot();
        assert_eq!(calls, 2);
        assert!(out > 0);
    }
}

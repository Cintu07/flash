//! The executor: what actually happens inside a node.
//!
//! Everything here is written so that a node is a pure function of its inputs. Read content by
//! hash, compute, write content by hash, return hashes. No node reads the filesystem, and no node
//! mutates shared state. That is what makes the memo store safe: a hit can stand in for an
//! execution because there was nothing else the execution could have done.
//!
//! The escalation ladder for a failed edit follows section 4.1 exactly:
//!
//! 1. entity ops
//! 2. entity ops again, with the diagnostics
//! 3. one unified diff for the file
//! 4. give up, and report it as a failure rather than a silent no-op
//!
//! Each step is a node, so a repair that has been done before is a cache hit, and the *rate* of
//! steps 3 and 4 is recorded because it is the primary metric for whether the op schema fits the
//! language at all.

use crate::model::{ModelClient, ModelRequest};
use crate::nodes::{EditMode, NodeOp, key};
use crate::prompts;
use flash_adapter::{Adapter, Artifact, Delta, Op, PackRequest, VerifyCtx};
use flash_core::{Attribution, Digest, Env, NodeKind, NodeOutput};
use flash_engine::{ExecCtx, ExecFuture, Expansion, NodeExecutor, NodeResult, NodeSpec};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Counters the benchmark reports (d11). Op resolve rate is phase 1's exit criterion and its kill
/// line, so it is counted here rather than inferred from logs later.
#[derive(Default, Debug)]
pub struct RuntimeMetrics {
    pub edits_attempted: AtomicU64,
    pub ops_applied: AtomicU64,
    pub ops_rejected: AtomicU64,
    pub diff_fallbacks: AtomicU64,
    pub repairs: AtomicU64,
    pub gave_up: AtomicU64,
    pub rungs_run: AtomicU64,
    pub rungs_failed: AtomicU64,
    pub rungs_unavailable: AtomicU64,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct MetricsSnapshot {
    pub edits_attempted: u64,
    pub ops_applied: u64,
    pub ops_rejected: u64,
    pub diff_fallbacks: u64,
    pub repairs: u64,
    pub gave_up: u64,
    pub rungs_run: u64,
    pub rungs_failed: u64,
    pub rungs_unavailable: u64,
}

impl MetricsSnapshot {
    /// The phase 1 exit criterion: "op resolve rate above 85 percent".
    pub fn op_resolve_rate(&self) -> f64 {
        let total = self.ops_applied + self.ops_rejected;
        if total == 0 {
            return 1.0;
        }
        self.ops_applied as f64 / total as f64
    }

    /// The phase 1 kill line reads on this: a high fallback rate means the schema is wrong for
    /// the language, not that the model is bad.
    pub fn fallback_rate(&self) -> f64 {
        if self.edits_attempted == 0 {
            return 0.0;
        }
        self.diff_fallbacks as f64 / self.edits_attempted as f64
    }
}

impl RuntimeMetrics {
    pub fn snapshot(&self) -> MetricsSnapshot {
        let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
        MetricsSnapshot {
            edits_attempted: g(&self.edits_attempted),
            ops_applied: g(&self.ops_applied),
            ops_rejected: g(&self.ops_rejected),
            diff_fallbacks: g(&self.diff_fallbacks),
            repairs: g(&self.repairs),
            gave_up: g(&self.gave_up),
            rungs_run: g(&self.rungs_run),
            rungs_failed: g(&self.rungs_failed),
            rungs_unavailable: g(&self.rungs_unavailable),
        }
    }
}

pub struct RuntimeConfig {
    /// Workspace root, for rungs that shell out.
    pub workspace: Option<PathBuf>,
    /// Rungs to run after each edit. Rung 4 is task level and is added by the planner.
    pub levels: Vec<u8>,
    pub budget_chars: usize,
    /// After this many failed attempts at one edit, stop. Section 4.1 says ops twice, then a
    /// diff, so three.
    pub max_attempts: u32,
    pub planner_model_id: String,
    pub executor_model_id: String,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        RuntimeConfig {
            workspace: None,
            levels: vec![0, 1],
            budget_chars: flash_adapter_code::pack::DEFAULT_BUDGET_CHARS,
            max_attempts: 3,
            planner_model_id: "planner".into(),
            executor_model_id: "executor".into(),
        }
    }
}

pub struct Runtime {
    adapters: Vec<Arc<dyn Adapter>>,
    planner: Arc<dyn ModelClient>,
    executor: Arc<dyn ModelClient>,
    cfg: RuntimeConfig,
    pub metrics: Arc<RuntimeMetrics>,
}

impl Runtime {
    pub fn new(
        adapters: Vec<Arc<dyn Adapter>>,
        planner: Arc<dyn ModelClient>,
        executor: Arc<dyn ModelClient>,
        cfg: RuntimeConfig,
    ) -> Arc<Self> {
        Arc::new(Runtime {
            adapters,
            planner,
            executor,
            cfg,
            metrics: Arc::new(RuntimeMetrics::default()),
        })
    }

    /// The environment a node runs in. Every field here is hashed into the action key, which is
    /// what makes "bump a prompt, invalidate only its nodes" true.
    pub fn env_for(&self, template: &str, adapter_version: &str, model_id: &str) -> Env {
        Env::new(model_id, prompts::version(template), adapter_version)
    }
}

fn fail(diagnostics: Vec<String>) -> NodeResult {
    NodeResult::done(NodeOutput::fail(diagnostics))
}

/// Read an artifact back out of the content store.
fn load_artifact(ctx: &ExecCtx, path: &str, digest: &Digest) -> Result<Artifact, String> {
    ctx.store
        .content
        .get(digest)
        .map(|bytes| Artifact::new(path.to_string(), bytes))
        .map_err(|e| format!("content {digest} is missing: {e}"))
}

impl NodeExecutor for Runtime {
    fn execute(&self, ctx: ExecCtx) -> ExecFuture {
        let this = self as *const Runtime;
        // Safety-free alternative: the engine holds the executor in an Arc for the whole run, so
        // borrowing it for the duration of one node is sound, but rather than reason about that,
        // clone the cheap handles the node actually needs.
        let _ = this;
        let adapters = self.adapters.clone();
        let planner = self.planner.clone();
        let executor = self.executor.clone();
        let metrics = self.metrics.clone();
        let workspace = self.cfg.workspace.clone();
        let levels = self.cfg.levels.clone();
        let budget_chars = self.cfg.budget_chars;
        let max_attempts = self.cfg.max_attempts;
        let planner_model_id = self.cfg.planner_model_id.clone();
        let executor_model_id = self.cfg.executor_model_id.clone();

        Box::pin(async move {
            let Some(op) = NodeOp::decode(&ctx.spec.op) else {
                return fail(vec!["node op payload is not a flash node op".into()]);
            };

            match op {
                // ---- a file, as it is on disk -------------------------------------------------
                NodeOp::Source { content, .. } => {
                    if !ctx.store.content.has(&content) {
                        return fail(vec![format!(
                            "source content {content} is not in the store"
                        )]);
                    }
                    NodeResult::done(NodeOutput::pass(vec![content]))
                }

                // ---- the structural summary a planner needs ----------------------------------
                NodeOp::Outline { path } => {
                    let Some(artifact_digest) = ctx.inputs.first() else {
                        return fail(vec!["outline node has no artifact input".into()]);
                    };
                    let artifact = match load_artifact(&ctx, &path, artifact_digest) {
                        Ok(a) => a,
                        Err(e) => return fail(vec![e]),
                    };
                    let Some(adapter) = flash_adapter::route(&adapters, &path) else {
                        return fail(vec![format!("no adapter handles {path}")]);
                    };
                    let outline = match adapter.outline(&artifact) {
                        Ok(o) => o,
                        Err(e) => return fail(vec![format!("{path} did not parse: {e}")]),
                    };
                    // Ids and kinds only. Bodies are deliberately excluded: that is what lets a
                    // body edit leave the plan untouched.
                    let mut lines: Vec<String> = outline
                        .entities
                        .iter()
                        .map(|e| format!("{} [{}]", e.id, e.kind))
                        .collect();
                    lines.sort();
                    let bytes = format!("{path}\n{}", lines.join("\n")).into_bytes();
                    match ctx.store.content.put(&bytes) {
                        Ok(d) => NodeResult::done(NodeOutput::pass(vec![d])),
                        Err(e) => fail(vec![format!("could not store the outline: {e}")]),
                    }
                }

                // ---- deterministic context assembly -------------------------------------------
                NodeOp::Pack {
                    path,
                    target,
                    budget_chars: budget,
                } => {
                    // The join node reuses this variant with a sentinel path; it exists only to
                    // give a multi-file plan one sink, and its output is the concatenation of
                    // whatever it waited on.
                    if path == "<join>" {
                        let mut h = flash_core::Hasher::new("flash.join.v1");
                        for d in &ctx.inputs {
                            h.digest(d);
                        }
                        return NodeResult::done(NodeOutput::pass(vec![h.finish()]));
                    }

                    let Some(artifact_digest) = ctx.inputs.first() else {
                        return fail(vec!["pack node has no artifact input".into()]);
                    };
                    let artifact = match load_artifact(&ctx, &path, artifact_digest) {
                        Ok(a) => a,
                        Err(e) => return fail(vec![e]),
                    };
                    let Some(adapter) = flash_adapter::route(&adapters, &path) else {
                        return fail(vec![format!("no adapter handles {path}")]);
                    };
                    let started = Instant::now();
                    let outline = match adapter.outline(&artifact) {
                        Ok(o) => o,
                        Err(e) => return fail(vec![format!("{path} did not parse: {e}")]),
                    };
                    let pack = match adapter.pack(&PackRequest {
                        artifact: &artifact,
                        outline: &outline,
                        target: &target,
                        budget_chars: budget.max(budget_chars.min(budget.max(1))),
                    }) {
                        Ok(p) => p,
                        Err(e) => return fail(vec![format!("could not pack {target}: {e}")]),
                    };
                    let bytes = pack.render().into_bytes();
                    match ctx.store.content.put(&bytes) {
                        Ok(d) => NodeResult::done(NodeOutput::pass(vec![d]).with_attribution(
                            Attribution {
                                apply_ms: started.elapsed().as_millis() as u64,
                                ..Default::default()
                            },
                        )),
                        Err(e) => fail(vec![format!("could not store pack: {e}")]),
                    }
                }

                // ---- an executor call, and the materialization of what it emitted -------------
                NodeOp::Edit {
                    path,
                    target,
                    instruction,
                    mode,
                    attempt,
                    diagnostics,
                } => {
                    metrics.edits_attempted.fetch_add(1, Ordering::Relaxed);

                    let Some(artifact_digest) = ctx.inputs.first().copied() else {
                        return fail(vec!["edit node has no artifact input".into()]);
                    };
                    let artifact = match load_artifact(&ctx, &path, &artifact_digest) {
                        Ok(a) => a,
                        Err(e) => return fail(vec![e]),
                    };
                    let pack_text = match ctx.inputs.get(1) {
                        Some(d) => ctx
                            .store
                            .content
                            .get(d)
                            .map(|b| String::from_utf8_lossy(&b).to_string())
                            .unwrap_or_default(),
                        None => String::new(),
                    };
                    let Some(adapter) = flash_adapter::route(&adapters, &path) else {
                        return fail(vec![format!("no adapter handles {path}")]);
                    };

                    let system = match (mode, attempt) {
                        (EditMode::Diff, _) => prompts::DIFF_SYSTEM,
                        (_, 0) => prompts::EDIT_SYSTEM,
                        _ => prompts::REPAIR_SYSTEM,
                    };
                    let req = ModelRequest {
                        model_id: executor_model_id.clone(),
                        prompt_version: prompts::version(system),
                        system: system.to_string(),
                        user: prompts::edit_user(&instruction, &pack_text, &diagnostics),
                        schema: Some(adapter.op_schema()),
                        max_output_tokens: 2048,
                    };

                    let decode_start = Instant::now();
                    let response = match executor.call(&req).await {
                        Ok(r) => r,
                        Err(e) => return fail(vec![format!("executor model failed: {e}")]),
                    };
                    let decode_ms = decode_start.elapsed().as_millis() as u64;

                    let ops: Vec<Op> = match response.json.as_ref().and_then(|v| v.as_array()) {
                        Some(items) => items.iter().cloned().map(Op::new).collect(),
                        None => {
                            metrics.ops_rejected.fetch_add(1, Ordering::Relaxed);
                            return escalate(
                                &metrics,
                                &ctx,
                                &path,
                                &target,
                                &instruction,
                                attempt,
                                max_attempts,
                                vec!["the model did not return a json array of ops".into()],
                            );
                        }
                    };

                    let apply_start = Instant::now();
                    match adapter.apply(&artifact, &ops) {
                        Ok(applied) => {
                            metrics.ops_applied.fetch_add(1, Ordering::Relaxed);
                            let apply_ms = apply_start.elapsed().as_millis() as u64;
                            match ctx.store.content.put(&applied.artifact.bytes) {
                                Ok(d) => NodeResult::done(
                                    NodeOutput::pass(vec![d]).with_attribution(Attribution {
                                        decode_ms,
                                        apply_ms,
                                        ..Default::default()
                                    }),
                                ),
                                Err(e) => {
                                    fail(vec![format!("could not store the edited file: {e}")])
                                }
                            }
                        }
                        Err(e) => {
                            metrics.ops_rejected.fetch_add(1, Ordering::Relaxed);
                            escalate(
                                &metrics,
                                &ctx,
                                &path,
                                &target,
                                &instruction,
                                attempt,
                                max_attempts,
                                vec![format!("the ops were rejected: {e}")],
                            )
                        }
                    }
                }

                // ---- one rung of the ladder ---------------------------------------------------
                NodeOp::Verify {
                    path,
                    level,
                    attempt,
                    target,
                    instruction,
                } => {
                    let Some(artifact_digest) = ctx.inputs.first().copied() else {
                        return fail(vec!["verify node has no artifact input".into()]);
                    };
                    let artifact = match load_artifact(&ctx, &path, &artifact_digest) {
                        Ok(a) => a,
                        Err(e) => return fail(vec![e]),
                    };
                    let Some(adapter) = flash_adapter::route(&adapters, &path) else {
                        return fail(vec![format!("no adapter handles {path}")]);
                    };
                    let outline = match adapter.outline(&artifact) {
                        Ok(o) => o,
                        Err(e) => {
                            // A file that will not parse fails rung 0 by definition.
                            metrics.rungs_failed.fetch_add(1, Ordering::Relaxed);
                            return verify_failed(
                                &metrics,
                                &ctx,
                                &path,
                                &target,
                                &instruction,
                                level,
                                attempt,
                                max_attempts,
                                vec![format!("{path} does not parse: {e}")],
                            );
                        }
                    };

                    // What changed, recomputed here from content rather than passed along. The
                    // second input is the artifact as it was before the edit; without it a rung
                    // cannot tell a deletion from a file that never had the function.
                    let delta = match ctx.inputs.get(1) {
                        Some(before_digest) => match load_artifact(&ctx, &path, before_digest) {
                            Ok(before) => match adapter.outline(&before) {
                                Ok(before_outline) => flash_adapter::delta_between(
                                    &before_outline,
                                    &outline,
                                    &before,
                                    &artifact,
                                ),
                                Err(_) => Delta::default(),
                            },
                            Err(_) => Delta::default(),
                        },
                        None => Delta::default(),
                    };
                    let impact = adapter.impact(&outline, &delta);
                    let Some(rung) = adapter.ladder().into_iter().find(|r| r.level() == level)
                    else {
                        return fail(vec![format!("no rung at level {level}")]);
                    };

                    let started = Instant::now();
                    let outcome = rung.check(&VerifyCtx {
                        artifact: &artifact,
                        outline: &outline,
                        delta: &delta,
                        impact: &impact,
                        workspace: workspace.as_deref(),
                    });
                    let verify_ms = started.elapsed().as_millis() as u64;
                    metrics.rungs_run.fetch_add(1, Ordering::Relaxed);

                    if outcome.unavailable {
                        // A rung that could not run is not a pass. Failing here is deliberate: it
                        // keeps unverified work out of a shared cache.
                        metrics.rungs_unavailable.fetch_add(1, Ordering::Relaxed);
                        return fail(
                            outcome
                                .diagnostics
                                .iter()
                                .map(|d| format!("{}: {}", d.code, d.message))
                                .collect(),
                        );
                    }

                    if outcome.passed {
                        // Pass the artifact through unchanged, so the chain stays content
                        // addressed and a rerun of a verified file is a hit.
                        return NodeResult::done(
                            NodeOutput::pass(vec![artifact_digest]).with_attribution(Attribution {
                                verify_ms,
                                ..Default::default()
                            }),
                        );
                    }

                    metrics.rungs_failed.fetch_add(1, Ordering::Relaxed);
                    verify_failed(
                        &metrics,
                        &ctx,
                        &path,
                        &target,
                        &instruction,
                        level,
                        attempt,
                        max_attempts,
                        outcome
                            .diagnostics
                            .iter()
                            .map(|d| match d.line {
                                Some(l) => format!("{} at line {l}: {}", d.code, d.message),
                                None => format!("{}: {}", d.code, d.message),
                            })
                            .collect(),
                    )
                }

                // ---- the planner --------------------------------------------------------------
                NodeOp::Plan { instruction, paths } => {
                    // inputs[i] is the outline of paths[i]: the planner sees structure, never
                    // bodies, so it is not re-invoked for an edit that changed no structure.
                    let mut outlines: Vec<(String, Vec<String>)> = Vec::new();
                    for (i, path) in paths.iter().enumerate() {
                        let Some(digest) = ctx.inputs.get(i) else {
                            continue;
                        };
                        let Ok(bytes) = ctx.store.content.get(digest) else {
                            continue;
                        };
                        let text = String::from_utf8_lossy(&bytes).to_string();
                        let ids: Vec<String> =
                            text.lines().skip(1).map(|l| l.to_string()).collect();
                        outlines.push((path.clone(), ids));
                    }

                    let req = ModelRequest {
                        model_id: planner_model_id.clone(),
                        prompt_version: prompts::version(prompts::PLAN_SYSTEM),
                        system: prompts::PLAN_SYSTEM.to_string(),
                        user: prompts::plan_user(&instruction, &outlines),
                        schema: Some(prompts::plan_schema()),
                        max_output_tokens: 1024,
                    };

                    let decode_start = Instant::now();
                    let response = match planner.call(&req).await {
                        Ok(r) => r,
                        Err(e) => return fail(vec![format!("planner model failed: {e}")]),
                    };
                    let decode_ms = decode_start.elapsed().as_millis() as u64;

                    let edits: Vec<crate::nodes::EditRequest> = match response
                        .json
                        .as_ref()
                        .and_then(|v| v.get("edits"))
                        .and_then(|v| serde_json::from_value(v.clone()).ok())
                    {
                        Some(e) => e,
                        None => {
                            return fail(vec!["planner did not return a valid edit list".into()]);
                        }
                    };
                    if edits.is_empty() {
                        return fail(vec!["planner returned no edits".into()]);
                    }

                    let env = Env::new(
                        executor_model_id.clone(),
                        prompts::version(prompts::EDIT_SYSTEM),
                        adapters
                            .first()
                            .map(|a| a.version().to_string())
                            .unwrap_or_else(|| "none".into()),
                    );
                    let chain = crate::nodes::build_chain(
                        &edits,
                        &crate::nodes::ChainConfig {
                            env,
                            levels: levels.clone(),
                            budget_chars,
                        },
                    );

                    let plan_digest = match ctx
                        .store
                        .content
                        .put(serde_json::to_string(&edits).unwrap_or_default().as_bytes())
                    {
                        Ok(d) => d,
                        Err(e) => return fail(vec![format!("could not store the plan: {e}")]),
                    };

                    let mut result = NodeResult::expand(
                        vec![plan_digest],
                        Expansion::new(chain.nodes, chain.tail),
                    );
                    result.output.attribution = Attribution {
                        decode_ms,
                        ..Default::default()
                    };
                    result
                }
            }
        })
    }
}

/// An edit whose ops were refused: try again, then fall back to a diff, then stop.
#[allow(clippy::too_many_arguments)]
fn escalate(
    metrics: &Arc<RuntimeMetrics>,
    ctx: &ExecCtx,
    path: &str,
    target: &str,
    instruction: &str,
    attempt: u32,
    max_attempts: u32,
    diagnostics: Vec<String>,
) -> NodeResult {
    if attempt + 1 >= max_attempts {
        metrics.gave_up.fetch_add(1, Ordering::Relaxed);
        return fail(diagnostics);
    }

    // Attempt 0 -> ops again with diagnostics. Attempt 1 -> the diff fallback.
    let next_mode = if attempt == 0 {
        EditMode::Ops
    } else {
        metrics.diff_fallbacks.fetch_add(1, Ordering::Relaxed);
        EditMode::Diff
    };
    metrics.repairs.fetch_add(1, Ordering::Relaxed);

    let retry = NodeSpec::new("retry", NodeKind::Op)
        .op(NodeOp::Edit {
            path: path.to_string(),
            target: target.to_string(),
            instruction: instruction.to_string(),
            mode: next_mode,
            attempt: attempt + 1,
            diagnostics,
        }
        .encode())
        .env(ctx.spec.env.clone())
        // The retry sees exactly what this attempt saw: same artifact, same pack.
        .deps(ctx.spec.deps.iter().map(|d| d.as_str().to_string()));

    NodeResult::expand(Vec::new(), Expansion::new(vec![retry], "retry"))
}

/// A rung that failed: emit a fix and a recheck, and hand identity to the recheck.
#[allow(clippy::too_many_arguments)]
fn verify_failed(
    metrics: &Arc<RuntimeMetrics>,
    ctx: &ExecCtx,
    path: &str,
    target: &str,
    instruction: &str,
    level: u8,
    attempt: u32,
    max_attempts: u32,
    diagnostics: Vec<String>,
) -> NodeResult {
    if attempt + 1 >= max_attempts {
        metrics.gave_up.fetch_add(1, Ordering::Relaxed);
        return fail(diagnostics);
    }
    metrics.repairs.fetch_add(1, Ordering::Relaxed);

    let mode = if attempt == 0 {
        EditMode::Ops
    } else {
        metrics.diff_fallbacks.fetch_add(1, Ordering::Relaxed);
        EditMode::Diff
    };

    // The fix reads the artifact this rung just rejected, so it is repairing what actually exists
    // rather than the state before the edit. Only the first dependency: an edit node takes the
    // artifact first and a pack second, and handing it the rung's "before" artifact as a pack
    // would feed the model a whole stale file where its context should be.
    let mut fix = NodeSpec::new("fix", NodeKind::Op)
        .op(NodeOp::Edit {
            path: path.to_string(),
            target: target.to_string(),
            instruction: instruction.to_string(),
            mode,
            attempt: attempt + 1,
            diagnostics: diagnostics.clone(),
        }
        .encode())
        .env(ctx.spec.env.clone());
    if let Some(current) = ctx.spec.deps.first() {
        fix = fix.dep(current.as_str());
    }

    // The recheck compares the repaired artifact against the same "before" the original rung used,
    // so the delta it sees is the whole edit plus its repair, not just the repair.
    let mut recheck = NodeSpec::new(key::verify(path, level).replace(':', "-"), NodeKind::Verify)
        .op(NodeOp::Verify {
            path: path.to_string(),
            level,
            attempt: attempt + 1,
            target: target.to_string(),
            instruction: instruction.to_string(),
        }
        .encode())
        .env(ctx.spec.env.clone())
        .dep("fix");
    if let Some(before) = ctx.spec.deps.get(1) {
        recheck = recheck.dep(before.as_str());
    }
    let recheck_key = recheck.key.as_str().to_string();

    NodeResult::expand(Vec::new(), Expansion::new(vec![fix, recheck], recheck_key))
}

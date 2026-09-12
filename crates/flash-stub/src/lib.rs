//! flash-stub: fake adapters and fake models, for phase 0.
//!
//! Phase 0 builds the engine with no models at all: "if you cannot get this right with stubs,
//! nothing else matters." A stub node is a recipe that says how long to take and what to emit,
//! so the engine's behaviour is checkable against an oracle the test computes independently.
//!
//! Two properties of the stubs are deliberate:
//!
//! * **Variance.** Real steps are heavy tailed: a render is usually 3 s and occasionally 9 s.
//!   Stubs with constant durations would make the "eta error under 20 percent" exit criterion
//!   pass without the eta being any good, so every stub draws from a distribution with a tail.
//! * **Constant nodes.** Some stubs ignore their inputs and emit fixed bytes, modelling a step
//!   like "re-summarize a table whose numbers did not actually move". These are what exercise
//!   early cutoff: they rerun, produce the same bytes, and their children stay hot.

use flash_core::{Attribution, Digest, Hasher, NodeKind, NodeOutput};
use flash_engine::{ExecCtx, ExecFuture, NodeExecutor, NodeSpec, Progress, TaskGraph};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// What a stub node does when it runs.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct StubOp {
    pub label: String,
    /// Median cost in milliseconds.
    pub p50_ms: u64,
    /// Spread as a fraction of p50, plus a rare multiplier, so the distribution has a tail.
    pub jitter: f32,
    /// Ignore inputs and emit fixed bytes. Models a step whose output did not move.
    pub constant: bool,
    /// Always fail. Models a ladder rung catching something.
    pub fail: bool,
    /// Which attribution bucket this node's time belongs in.
    pub bucket: Bucket,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Bucket {
    Decode,
    Verify,
    Apply,
    Render,
}

impl StubOp {
    pub fn new(label: impl Into<String>) -> Self {
        StubOp {
            label: label.into(),
            p50_ms: 5,
            jitter: 0.25,
            constant: false,
            fail: false,
            bucket: Bucket::Decode,
        }
    }

    pub fn cost(mut self, p50_ms: u64) -> Self {
        self.p50_ms = p50_ms;
        self
    }

    pub fn jitter(mut self, j: f32) -> Self {
        self.jitter = j;
        self
    }

    pub fn constant(mut self) -> Self {
        self.constant = true;
        self
    }

    pub fn failing(mut self) -> Self {
        self.fail = true;
        self
    }

    pub fn bucket(mut self, b: Bucket) -> Self {
        self.bucket = b;
        self
    }

    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("stub op is serializable")
    }

    pub fn decode(bytes: &[u8]) -> Option<StubOp> {
        serde_json::from_slice(bytes).ok()
    }
}

/// A node backed by a stub op.
pub fn stub_node(key: impl Into<String>, kind: NodeKind, op: StubOp) -> NodeSpec {
    NodeSpec::new(key, kind).op(op.encode())
}

/// Deterministic, seedable rng. Written here rather than pulled in so that a test failure can
/// always be replayed from its seed with no dependency drift.
#[derive(Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next_u64() % n as u64) as usize
        }
    }

    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    pub fn chance(&mut self, p: f64) -> bool {
        self.unit() < p
    }
}

/// Draw a duration with a heavy tail: mostly p50 +/- jitter, occasionally several times longer.
fn draw_ms(op: &StubOp, rng: &mut Rng) -> u64 {
    let base = op.p50_ms as f64;
    let spread = base * op.jitter as f64;
    let mut ms = base + (rng.unit() * 2.0 - 1.0) * spread;
    if rng.chance(0.07) {
        ms *= 2.0 + rng.unit() * 2.0; // the tail: one in fourteen runs is 2x to 4x
    }
    ms.max(0.0).round() as u64
}

/// The fake model and fake adapter behind phase 0.
///
/// Counts executions so a test can assert not just "the answer is right" but "the work that was
/// avoided was actually avoided", which is the only claim that matters here.
pub struct StubExecutor {
    calls: AtomicU64,
    seed: AtomicU64,
    /// Scale every sleep. Tests run at 0 to stay fast; the demo runs at 1.
    time_scale: f64,
}

impl StubExecutor {
    pub fn new() -> Arc<Self> {
        Arc::new(StubExecutor {
            calls: AtomicU64::new(0),
            seed: AtomicU64::new(0x5eed),
            time_scale: 1.0,
        })
    }

    /// An executor that computes honestly but does not actually sleep. Durations are still
    /// *reported*, so eta and history logic is exercised without the wall clock cost.
    pub fn instant() -> Arc<Self> {
        Arc::new(StubExecutor {
            calls: AtomicU64::new(0),
            seed: AtomicU64::new(0x5eed),
            time_scale: 0.0,
        })
    }

    /// Run at a fraction of the modelled durations, so a demo of a 90 second task fits in a
    /// few seconds without changing any of the ratios between steps.
    pub fn scaled(time_scale: f64) -> Arc<Self> {
        Arc::new(StubExecutor {
            calls: AtomicU64::new(0),
            seed: AtomicU64::new(0x5eed),
            time_scale,
        })
    }

    pub fn calls(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }

    pub fn reset_calls(&self) {
        self.calls.store(0, Ordering::Relaxed);
    }
}

impl NodeExecutor for StubExecutor {
    fn execute(&self, ctx: ExecCtx) -> ExecFuture {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let seed = self.seed.fetch_add(0x9e37_79b9, Ordering::Relaxed);
        let scale = self.time_scale;
        Box::pin(async move {
            let op = match StubOp::decode(&ctx.spec.op) {
                Some(op) => op,
                // A source node's op is raw content, not a stub recipe. It simply is its content.
                None => {
                    let d = Digest::of(&ctx.spec.op);
                    return NodeOutput::pass(vec![d]).into();
                }
            };

            let mut rng = Rng::new(seed ^ ctx.action.0.as_bytes()[0] as u64);
            let modelled_ms = draw_ms(&op, &mut rng);
            // Attribution reports what actually elapsed. Reporting the modelled duration while
            // the wall clock shows the scaled one makes a demo where the parts do not add up to
            // the whole, and a benchmark table nobody can check.
            let ms = if scale > 0.0 {
                (modelled_ms as f64 * scale).round() as u64
            } else {
                modelled_ms
            };

            if scale > 0.0 {
                let sleep_ms = ms;
                if let Some(job) = &ctx.job {
                    // A job reports progress in phases and watches for cancellation, which is
                    // what makes "close the laptop, the daemon keeps working" observable.
                    let steps = 4u64;
                    for i in 1..=steps {
                        if job.cancelled() {
                            return NodeOutput::fail(vec!["cancelled".to_string()]).into();
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(sleep_ms / steps))
                            .await;
                        job.report(Progress::at(i as f32 / steps as f32, op.label.clone()));
                    }
                } else {
                    tokio::time::sleep(std::time::Duration::from_millis(sleep_ms)).await;
                }
            }

            if op.fail {
                return NodeOutput::fail(vec![format!("{}: stub failure", op.label)]).into();
            }

            // The output: fixed bytes for a constant node, otherwise a function of the inputs.
            let mut h = Hasher::new("flash.stub.output.v1");
            h.str(&op.label);
            if !op.constant {
                for d in &ctx.inputs {
                    h.digest(d);
                }
            }
            let out = h.finish();

            let mut attribution = Attribution::default();
            match op.bucket {
                Bucket::Decode => attribution.decode_ms = ms,
                Bucket::Verify => attribution.verify_ms = ms,
                Bucket::Apply => attribution.apply_ms = ms,
                Bucket::Render => attribution.render_ms = ms,
            }
            NodeOutput::pass(vec![out])
                .with_attribution(attribution)
                .into()
        })
    }
}

/// A randomly shaped but reproducible task graph, for the 100-synthetic-graphs exit criterion.
pub struct SyntheticGraph {
    pub graph: TaskGraph,
    pub sources: Vec<String>,
    pub keys: Vec<String>,
    /// Nodes that ignore their inputs. Their children survive an upstream change untouched.
    pub constants: Vec<String>,
}

/// Build a layered DAG: a few sources, then layers of nodes each depending on 1 to 3 earlier ones.
pub fn synthetic_graph(seed: u64, nodes: usize) -> SyntheticGraph {
    let mut rng = Rng::new(seed);
    let mut graph = TaskGraph::new();
    let mut keys: Vec<String> = Vec::new();
    let mut sources = Vec::new();
    let mut constants = Vec::new();

    let n_sources = 1 + rng.below(3);
    for i in 0..n_sources {
        let key = format!("src:{i}");
        graph
            .add(NodeSpec::source(
                key.clone(),
                Digest::of(format!("seed{seed}/src{i}").as_bytes()),
            ))
            .expect("unique source key");
        sources.push(key.clone());
        keys.push(key);
    }

    for i in 0..nodes {
        let key = format!("n:{i}");
        let kind = match rng.below(5) {
            0 => NodeKind::Context,
            1 => NodeKind::Op,
            2 => NodeKind::Verify,
            3 => NodeKind::Data,
            _ => NodeKind::Op,
        };
        let constant = rng.chance(0.15);
        let mut op = StubOp::new(key.clone()).cost(1 + rng.below(8) as u64);
        if constant {
            op = op.constant();
            constants.push(key.clone());
        }
        let mut spec = stub_node(key.clone(), kind, op);
        let fanin = 1 + rng.below(3);
        let mut picked: Vec<String> = Vec::new();
        for _ in 0..fanin {
            let cand = keys[rng.below(keys.len())].clone();
            if !picked.contains(&cand) {
                picked.push(cand);
            }
        }
        for p in picked {
            spec = spec.dep(p);
        }
        graph.add(spec).expect("unique node key");
        keys.push(key);
    }

    SyntheticGraph {
        graph,
        sources,
        keys,
        constants,
    }
}

/// Change one source's content, the way an edited file or a new csv would.
pub fn mutate_source(graph: &mut TaskGraph, source: &str, marker: &str) {
    graph.replace(NodeSpec::source(
        source.to_string(),
        Digest::of(marker.as_bytes()),
    ));
}

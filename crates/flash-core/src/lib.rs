//! flash-core: identities, hashing, and the vocabulary every other crate speaks.
//!
//! Two identities matter and they are not the same thing:
//!
//! * [`NodeKey`] is the *logical* identity of a step in a task graph ("section:methodology").
//!   It is stable across runs and is what duration history is keyed on.
//! * [`ActionKey`] is the *content-addressed* identity of one concrete execution: the op, the
//!   environment it runs in, and the content hashes of the outputs its inputs actually produced.
//!   It is the memo key.
//!
//! The distinction is the whole point. An action key can only be computed once a node's inputs
//! have been resolved to content, which is what gives early cutoff: a node that reruns and
//! produces byte-identical output leaves every downstream action key unchanged, so the
//! downstream stays hot even though its parent was recomputed.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// A 32-byte blake3 digest of some content.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Digest([u8; 32]);

impl Digest {
    pub const ZERO: Digest = Digest([0u8; 32]);

    pub fn of(bytes: &[u8]) -> Self {
        Digest(*blake3::hash(bytes).as_bytes())
    }

    pub fn from_bytes(b: [u8; 32]) -> Self {
        Digest(b)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn hex(&self) -> String {
        blake3::Hash::from(self.0).to_hex().to_string()
    }

    /// First 12 hex chars. Enough to read in a log line, never used as an identity.
    pub fn short(&self) -> String {
        self.hex()[..12].to_string()
    }

    pub fn from_hex(s: &str) -> Option<Self> {
        blake3::Hash::from_hex(s)
            .ok()
            .map(|h| Digest(*h.as_bytes()))
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.short())
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.hex())
    }
}

impl Serialize for Digest {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.hex())
    }
}

impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Digest::from_hex(&s).ok_or_else(|| serde::de::Error::custom("malformed digest"))
    }
}

/// Domain-separated, length-prefixed hasher.
///
/// Every field is framed with its length so that `("ab", "c")` and `("a", "bc")` cannot collide.
/// A collision here is cache poisoning, so the framing is not optional.
pub struct Hasher {
    inner: blake3::Hasher,
}

impl Hasher {
    pub fn new(domain: &str) -> Self {
        let mut inner = blake3::Hasher::new();
        inner.update(&(domain.len() as u64).to_le_bytes());
        inner.update(domain.as_bytes());
        Hasher { inner }
    }

    pub fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.inner.update(&(b.len() as u64).to_le_bytes());
        self.inner.update(b);
        self
    }

    pub fn str(&mut self, s: &str) -> &mut Self {
        self.bytes(s.as_bytes())
    }

    pub fn digest(&mut self, d: &Digest) -> &mut Self {
        self.bytes(d.as_bytes())
    }

    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.bytes(&v.to_le_bytes())
    }

    pub fn finish(&self) -> Digest {
        Digest(*self.inner.finalize().as_bytes())
    }
}

/// Lowercase hex. Used to carry opaque op payloads through json without ballooning them into
/// arrays of integers.
pub fn hex_encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 0x0f) as usize] as char);
    }
    out
}

pub fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 2);
    for pair in b.chunks(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    Some(out)
}

/// Serde support for a `Vec<u8>` field that should travel as a hex string.
pub mod hex_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&super::hex_encode(v))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        super::hex_decode(&s).ok_or_else(|| serde::de::Error::custom("malformed hex payload"))
    }
}

/// Stable logical identity of a node within a task graph.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct NodeKey(pub String);

impl NodeKey {
    pub fn new(s: impl Into<String>) -> Self {
        NodeKey(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Digest of the logical key, used to shard duration history on disk.
    pub fn digest(&self) -> Digest {
        Digest::of(self.0.as_bytes())
    }
}

impl fmt::Debug for NodeKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl fmt::Display for NodeKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Content-addressed identity of one execution. The memo key.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Serialize, Deserialize)]
pub struct ActionKey(pub Digest);

impl fmt::Display for ActionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// What kind of work a node does. Drives the job threshold and the concurrency lane.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeKind {
    /// Planner call: reads a pack, emits a subgraph.
    Plan,
    /// Deterministic context pack assembly.
    Context,
    /// Deterministic data transform (parse a csv, summarize a table).
    Data,
    /// Executor call: emits ops, which a materializer applies.
    Op,
    /// A verify rung.
    Verify,
    /// A render or export (docx to pdf, pptx, xlsx).
    Render,
    /// A build or other long external process.
    Build,
}

impl NodeKind {
    /// Kinds that are jobs regardless of what history says, per d5.
    pub fn is_inherently_slow(&self) -> bool {
        matches!(self, NodeKind::Render | NodeKind::Build)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            NodeKind::Plan => "plan",
            NodeKind::Context => "context",
            NodeKind::Data => "data",
            NodeKind::Op => "op",
            NodeKind::Verify => "verify",
            NodeKind::Render => "render",
            NodeKind::Build => "build",
        }
    }
}

/// A resource class. Concurrency is capped per lane so cpu-heavy work cannot starve the box.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct Lane(pub String);

impl Lane {
    pub fn new(s: impl Into<String>) -> Self {
        Lane(s.into())
    }

    /// Network-bound model calls. Cheap to run many at once.
    pub fn model() -> Self {
        Lane::new("model")
    }

    /// Cpu-bound local work. Capped near core count.
    pub fn cpu() -> Self {
        Lane::new("cpu")
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Everything outside the op itself that can change an answer.
///
/// `prompt_version` is a hash of the prompt template (d4), so bumping one prompt invalidates
/// exactly the nodes that used it and nothing else.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct Env {
    pub model_id: String,
    pub prompt_version: String,
    pub adapter_version: String,
}

impl Env {
    pub fn new(
        model_id: impl Into<String>,
        prompt_version: impl Into<String>,
        adapter_version: impl Into<String>,
    ) -> Self {
        Env {
            model_id: model_id.into(),
            prompt_version: prompt_version.into(),
            adapter_version: adapter_version.into(),
        }
    }

    /// Environment for nodes that never call a model: deterministic local work.
    pub fn deterministic(adapter_version: impl Into<String>) -> Self {
        Env::new("-", "-", adapter_version)
    }
}

/// Compute the action key for one execution.
///
/// `inputs` are the content hashes the node's dependencies actually produced, in declared
/// dependency order. Order is semantic: swapping two inputs is a different action.
pub fn action_key(kind: NodeKind, op: &[u8], env: &Env, inputs: &[Digest]) -> ActionKey {
    let mut h = Hasher::new("flash.action.v1");
    h.str(kind.as_str());
    h.bytes(op);
    h.str(&env.model_id);
    h.str(&env.prompt_version);
    h.str(&env.adapter_version);
    h.u64(inputs.len() as u64);
    for d in inputs {
        h.digest(d);
    }
    ActionKey(h.finish())
}

/// Did a node's ladder pass?
///
/// This gates memoization. A failed node is never written to the memo store: caching a wrong
/// answer would freeze one bad sample forever, and under a shared team cache it would spread.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Pass,
    Fail,
}

impl Verdict {
    pub fn passed(&self) -> bool {
        matches!(self, Verdict::Pass)
    }
}

/// Where the wall clock went inside one node. This is the unit the benchmark aggregates (d11).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Attribution {
    pub decode_ms: u64,
    pub verify_ms: u64,
    pub apply_ms: u64,
    pub render_ms: u64,
    pub wait_ms: u64,
}

impl Attribution {
    pub fn total_ms(&self) -> u64 {
        self.decode_ms + self.verify_ms + self.apply_ms + self.render_ms + self.wait_ms
    }

    pub fn merge(&mut self, other: &Attribution) {
        self.decode_ms += other.decode_ms;
        self.verify_ms += other.verify_ms;
        self.apply_ms += other.apply_ms;
        self.render_ms += other.render_ms;
        self.wait_ms += other.wait_ms;
    }
}

/// What a node produced.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeOutput {
    /// Content hashes of the artifacts this node produced, in a stable order.
    pub outputs: Vec<Digest>,
    pub verdict: Verdict,
    pub attribution: Attribution,
    /// Structured diagnostics, capped at 20 by convention (section 5). Never raw tool output.
    pub diagnostics: Vec<String>,
}

impl NodeOutput {
    pub fn pass(outputs: Vec<Digest>) -> Self {
        NodeOutput {
            outputs,
            verdict: Verdict::Pass,
            attribution: Attribution::default(),
            diagnostics: Vec::new(),
        }
    }

    pub fn fail(diagnostics: Vec<String>) -> Self {
        NodeOutput {
            outputs: Vec::new(),
            verdict: Verdict::Fail,
            attribution: Attribution::default(),
            diagnostics,
        }
    }

    pub fn with_attribution(mut self, a: Attribution) -> Self {
        self.attribution = a;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing_prevents_concatenation_collisions() {
        let mut a = Hasher::new("t");
        a.str("ab").str("c");
        let mut b = Hasher::new("t");
        b.str("a").str("bc");
        assert_ne!(a.finish(), b.finish());
    }

    #[test]
    fn domain_separation_holds() {
        let mut a = Hasher::new("one");
        a.str("x");
        let mut b = Hasher::new("two");
        b.str("x");
        assert_ne!(a.finish(), b.finish());
    }

    #[test]
    fn action_key_is_input_order_sensitive() {
        let env = Env::deterministic("v1");
        let x = Digest::of(b"x");
        let y = Digest::of(b"y");
        let k1 = action_key(NodeKind::Op, b"op", &env, &[x, y]);
        let k2 = action_key(NodeKind::Op, b"op", &env, &[y, x]);
        assert_ne!(k1, k2);
    }

    #[test]
    fn prompt_version_bump_changes_the_action() {
        let x = Digest::of(b"x");
        let a = action_key(NodeKind::Op, b"op", &Env::new("m", "p1", "v1"), &[x]);
        let b = action_key(NodeKind::Op, b"op", &Env::new("m", "p2", "v1"), &[x]);
        assert_ne!(a, b);
    }

    #[test]
    fn hex_roundtrips_and_rejects_junk() {
        let b = vec![0u8, 15, 16, 255, 42];
        assert_eq!(hex_decode(&hex_encode(&b)), Some(b));
        assert_eq!(hex_decode("abc"), None);
        assert_eq!(hex_decode("zz"), None);
    }

    #[test]
    fn digest_hex_roundtrips() {
        let d = Digest::of(b"hello");
        assert_eq!(Digest::from_hex(&d.hex()), Some(d));
    }
}

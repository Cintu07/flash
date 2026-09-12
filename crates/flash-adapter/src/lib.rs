//! flash-adapter: the one trait every artifact type implements (PRD section 4).
//!
//! An adapter supplies five things, and the engine supplies none of them:
//!
//! * a **document model**: how bytes parse into named entities with stable ids;
//! * an **op schema**: what the executor model is allowed to emit;
//! * a **materializer**: how ops become new bytes, deterministically;
//! * a **verify ladder**: cheapest check first, most expensive once at the end;
//! * **impact analysis**: given what changed, what actually has to be re-checked.
//!
//! The deliberate shape here is that adapters transform *content*, not a shared mutable model.
//! An adapter takes bytes and ops and returns new bytes. That is what lets every step be a
//! content-addressed node without the engine knowing what a function or a slide is.
//!
//! ## Why entity ids are the contract
//!
//! Everything downstream - packs, impact, diagnostics, caching - hangs off stable entity ids.
//! `fn:parse_header` has to mean the same thing before and after an edit somewhere else in the
//! file, or impact analysis degrades to "re-check everything" and warm collapses into cold.
//! Adapters must derive ids from names and structure, never from byte offsets or ordinals.

use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    #[error("{0}")]
    Parse(String),
    #[error("op rejected: {0}")]
    BadOp(String),
    #[error("no entity called {0}")]
    UnknownEntity(String),
    #[error("{0}")]
    Io(String),
}

pub type Result<T> = std::result::Result<T, AdapterError>;

/// One file's worth of content.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    /// Workspace-relative path. Part of an entity's identity, so it must be stable.
    pub path: String,
    pub bytes: Vec<u8>,
}

impl Artifact {
    pub fn new(path: impl Into<String>, bytes: impl Into<Vec<u8>>) -> Self {
        Artifact {
            path: path.into(),
            bytes: bytes.into(),
        }
    }

    pub fn text(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.bytes)
    }

    pub fn digest(&self) -> flash_core::Digest {
        let mut h = flash_core::Hasher::new("flash.artifact.v1");
        h.str(&self.path);
        h.bytes(&self.bytes);
        h.finish()
    }
}

/// A named thing inside an artifact: a function, a block, a slide, a range.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entity {
    /// Stable id, derived from names and structure. Never from byte offsets.
    pub id: String,
    /// Adapter-specific kind: "fn", "struct", "test", "heading", "slide", "range".
    pub kind: String,
    /// Byte range in the artifact.
    pub start: usize,
    pub end: usize,
    /// Byte range of the part that can be replaced independently (a function body, a block's
    /// text). None when the entity has no separable interior.
    pub body: Option<(usize, usize)>,
    /// One-line summary: a signature, a heading, a formula.
    pub signature: Option<String>,
    /// Ids this entity refers to. The edges impact analysis walks.
    pub refs: Vec<String>,
    /// Enclosing entity, if any.
    pub parent: Option<String>,
}

impl Entity {
    pub fn slice<'a>(&self, artifact: &'a Artifact) -> &'a [u8] {
        &artifact.bytes[self.start.min(artifact.bytes.len())..self.end.min(artifact.bytes.len())]
    }
}

/// The structural view of one artifact.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Outline {
    pub path: String,
    pub entities: Vec<Entity>,
}

impl Outline {
    pub fn get(&self, id: &str) -> Option<&Entity> {
        self.entities.iter().find(|e| e.id == id)
    }

    pub fn ids(&self) -> Vec<&str> {
        self.entities.iter().map(|e| e.id.as_str()).collect()
    }

    pub fn of_kind<'a>(&'a self, kind: &'a str) -> impl Iterator<Item = &'a Entity> {
        self.entities.iter().filter(move |e| e.kind == kind)
    }

    /// Everything that refers to `id`, directly.
    pub fn referrers(&self, id: &str) -> Vec<&Entity> {
        self.entities
            .iter()
            .filter(|e| e.refs.iter().any(|r| r == id))
            .collect()
    }
}

/// One structured edit. The payload is validated against the adapter's schema before it is
/// applied, and never interpreted as free text.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Op(pub serde_json::Value);

impl Op {
    pub fn new(v: serde_json::Value) -> Self {
        Op(v)
    }

    /// The discriminator every op schema shares.
    pub fn kind(&self) -> Option<&str> {
        self.0.get("op")?.as_str()
    }

    pub fn str_field(&self, name: &str) -> Result<&str> {
        self.0
            .get(name)
            .and_then(|v| v.as_str())
            .ok_or_else(|| AdapterError::BadOp(format!("missing string field `{name}`")))
    }

    pub fn opt_str(&self, name: &str) -> Option<&str> {
        self.0.get(name).and_then(|v| v.as_str())
    }
}

/// What changed, in entity terms rather than byte terms.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delta {
    pub changed: Vec<String>,
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

impl Delta {
    pub fn touched(&self) -> Vec<&str> {
        self.changed
            .iter()
            .chain(self.added.iter())
            .chain(self.removed.iter())
            .map(|s| s.as_str())
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.changed.is_empty() && self.added.is_empty() && self.removed.is_empty()
    }

    pub fn merge(&mut self, other: &Delta) {
        for (dst, src) in [
            (&mut self.changed, &other.changed),
            (&mut self.added, &other.added),
            (&mut self.removed, &other.removed),
        ] {
            for id in src {
                if !dst.contains(id) {
                    dst.push(id.clone());
                }
            }
        }
    }
}

/// The result of materializing ops.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Applied {
    pub artifact: Artifact,
    pub delta: Delta,
}

/// What has to be re-checked after a delta. This is what makes warm cheap (d7).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImpactSet {
    /// Entities whose correctness may have changed.
    pub entities: Vec<String>,
    /// Tests worth running, by adapter-specific selector.
    pub tests: Vec<String>,
    /// Render units (pdf pages, slide thumbnails, sheet ranges) that must be redone.
    pub units: Vec<String>,
}

impl ImpactSet {
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty() && self.tests.is_empty() && self.units.is_empty()
    }
}

/// A structured finding. Diagnostics go back to the model as items, never as raw tool output,
/// and are capped (section 5: max 20).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    /// Machine code where the tool gives one: "E0308", "heading-order", "overflow".
    pub code: String,
    pub message: String,
    /// Which entity it lands on, when that is knowable.
    pub entity: Option<String>,
    pub line: Option<u32>,
}

impl Diagnostic {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Diagnostic {
            code: code.into(),
            message: message.into(),
            entity: None,
            line: None,
        }
    }

    pub fn at_entity(mut self, e: impl Into<String>) -> Self {
        self.entity = Some(e.into());
        self
    }

    pub fn at_line(mut self, l: u32) -> Self {
        self.line = Some(l);
        self
    }
}

/// Section 5's cap, applied in one place so no adapter forgets it.
pub const MAX_DIAGNOSTICS: usize = 20;

pub fn cap_diagnostics(mut d: Vec<Diagnostic>) -> Vec<Diagnostic> {
    d.truncate(MAX_DIAGNOSTICS);
    d
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RungOutcome {
    pub passed: bool,
    pub diagnostics: Vec<Diagnostic>,
    /// True when the rung could not run at all (toolchain missing). Distinct from failing:
    /// a rung that cannot run must not be reported as a pass, and must not be cached.
    pub unavailable: bool,
}

impl RungOutcome {
    pub fn pass() -> Self {
        RungOutcome {
            passed: true,
            diagnostics: Vec::new(),
            unavailable: false,
        }
    }

    pub fn fail(diagnostics: Vec<Diagnostic>) -> Self {
        RungOutcome {
            passed: false,
            diagnostics: cap_diagnostics(diagnostics),
            unavailable: false,
        }
    }

    pub fn unavailable(why: impl Into<String>) -> Self {
        RungOutcome {
            passed: false,
            diagnostics: vec![Diagnostic::new("rung-unavailable", why)],
            unavailable: true,
        }
    }
}

/// What a rung gets to look at.
pub struct VerifyCtx<'a> {
    pub artifact: &'a Artifact,
    pub outline: &'a Outline,
    pub delta: &'a Delta,
    pub impact: &'a ImpactSet,
    /// Directory the artifact lives in, for rungs that shell out to a toolchain.
    pub workspace: Option<&'a std::path::Path>,
}

/// One rung of a verify ladder.
///
/// Rungs are ordered by `level` and run cheapest first, stopping at the first failure (section 5).
/// `expected_ms` is what the engine uses to decide whether the rung is a job.
pub trait VerifyRung: Send + Sync {
    fn name(&self) -> &str;
    fn level(&self) -> u8;
    fn expected_ms(&self) -> u64;
    /// Run once per task rather than per edit (rung 4).
    fn once_per_task(&self) -> bool {
        false
    }
    fn check(&self, ctx: &VerifyCtx<'_>) -> RungOutcome;
}

/// What a context pack should contain (section 3.3).
pub struct PackRequest<'a> {
    pub artifact: &'a Artifact,
    pub outline: &'a Outline,
    /// The entity being worked on.
    pub target: &'a str,
    /// Rough character budget. Packs are deterministic, so this is a hard trim, not a ranking.
    pub budget_chars: usize,
}

/// A deterministic context pack. Same inputs, same pack, so it caches like anything else.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pack {
    /// Named sections, in a fixed order. Named so the prompt can lay them out consistently and
    /// so a diff of two packs is readable.
    pub parts: Vec<(String, String)>,
}

impl Pack {
    pub fn push(&mut self, name: impl Into<String>, body: impl Into<String>) {
        let body = body.into();
        if !body.trim().is_empty() {
            self.parts.push((name.into(), body));
        }
    }

    pub fn render(&self) -> String {
        let mut s = String::new();
        for (name, body) in &self.parts {
            s.push_str("## ");
            s.push_str(name);
            s.push('\n');
            s.push_str(body.trim_end());
            s.push_str("\n\n");
        }
        s
    }

    pub fn chars(&self) -> usize {
        self.parts.iter().map(|(n, b)| n.len() + b.len()).sum()
    }
}

/// The trait from PRD section 4.
pub trait Adapter: Send + Sync + 'static {
    fn name(&self) -> &'static str;

    /// Bumped whenever parsing, ops or materialization change meaning. It is hashed into every
    /// action key, so bumping it invalidates exactly this adapter's nodes and nothing else.
    fn version(&self) -> &'static str;

    /// Does this adapter handle this path?
    fn handles(&self, path: &str) -> bool;

    /// What the executor model may emit, as a json schema.
    fn op_schema(&self) -> serde_json::Value;

    /// Parse into a typed structure.
    fn outline(&self, artifact: &Artifact) -> Result<Outline>;

    /// Deterministically apply ops. All or nothing: a rejected op leaves the artifact untouched.
    fn apply(&self, artifact: &Artifact, ops: &[Op]) -> Result<Applied>;

    fn ladder(&self) -> Vec<Arc<dyn VerifyRung>>;

    fn impact(&self, outline: &Outline, delta: &Delta) -> ImpactSet;

    fn pack(&self, req: &PackRequest<'_>) -> Result<Pack>;
}

/// What changed between two versions of one artifact, in entity terms.
///
/// Every adapter needs exactly this and they all computed it themselves, which is how three
/// copies of one function end up drifting. It belongs here: an entity is changed when it exists
/// on both sides and its bytes differ, added when it is new, removed when it is gone.
pub fn delta_between(
    before: &Outline,
    after: &Outline,
    before_art: &Artifact,
    after_art: &Artifact,
) -> Delta {
    let mut delta = Delta::default();
    for e in &after.entities {
        match before.get(&e.id) {
            None => delta.added.push(e.id.clone()),
            Some(old) => {
                // Byte ranges are meaningless for adapters that do not slice (sheets, slides), so
                // fall back to comparing the one-line signature they do carry.
                let moved = if old.end > old.start && e.end > e.start {
                    old.slice(before_art) != e.slice(after_art)
                } else {
                    old.signature != e.signature
                };
                if moved {
                    delta.changed.push(e.id.clone());
                }
            }
        }
    }
    for e in &before.entities {
        if after.get(&e.id).is_none() {
            delta.removed.push(e.id.clone());
        }
    }
    delta
}

/// Pick the adapter that handles a path.
pub fn route<'a>(adapters: &'a [Arc<dyn Adapter>], path: &str) -> Option<&'a Arc<dyn Adapter>> {
    adapters.iter().find(|a| a.handles(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_digest_covers_path_and_bytes() {
        let a = Artifact::new("src/lib.rs", b"x".to_vec());
        let b = Artifact::new("src/main.rs", b"x".to_vec());
        let c = Artifact::new("src/lib.rs", b"y".to_vec());
        assert_ne!(a.digest(), b.digest(), "path is part of identity");
        assert_ne!(a.digest(), c.digest());
        assert_eq!(
            a.digest(),
            Artifact::new("src/lib.rs", b"x".to_vec()).digest()
        );
    }

    #[test]
    fn diagnostics_are_capped() {
        let many: Vec<Diagnostic> = (0..50)
            .map(|i| Diagnostic::new("x", i.to_string()))
            .collect();
        assert_eq!(cap_diagnostics(many).len(), MAX_DIAGNOSTICS);
    }

    #[test]
    fn a_rung_that_cannot_run_is_not_a_pass() {
        let o = RungOutcome::unavailable("cargo not on PATH");
        assert!(!o.passed);
        assert!(o.unavailable);
    }

    #[test]
    fn pack_render_is_stable_and_skips_empty_parts() {
        let mut p = Pack::default();
        p.push("target", "fn a() {}");
        p.push("callers", "   ");
        p.push("imports", "use std::io;");
        let text = p.render();
        assert!(text.starts_with("## target"));
        assert!(!text.contains("callers"), "empty parts are dropped");
        assert_eq!(p.parts.len(), 2);
    }

    #[test]
    fn delta_merge_is_idempotent() {
        let mut a = Delta {
            changed: vec!["x".into()],
            ..Default::default()
        };
        let b = Delta {
            changed: vec!["x".into(), "y".into()],
            added: vec!["z".into()],
            ..Default::default()
        };
        a.merge(&b);
        a.merge(&b);
        assert_eq!(a.changed, vec!["x", "y"]);
        assert_eq!(a.added, vec!["z"]);
    }
}

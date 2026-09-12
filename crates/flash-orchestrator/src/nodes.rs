//! What a node *is*, in a real task.
//!
//! The engine hashes op payloads and knows nothing about them. This module is where those bytes
//! get meaning: a node is a source file, a context pack, an executor call that emits ops, a
//! verify rung, or a planner call that expands into all of the above.
//!
//! Node keys are chosen to be stable across runs of the same task, because duration history is
//! keyed on them (`verify:src/lib.rs@2` costs about the same every time, whatever the file
//! contains). Action keys, which decide caching, come from content and are computed by the
//! engine; nothing here tries to predict them.

use flash_core::{Digest, Env, NodeKind};
use flash_engine::NodeSpec;
use serde::{Deserialize, Serialize};

/// How an edit should be expressed. Section 4.1: entity ops, falling back to a diff after two
/// failures, with the fallback rate as a primary metric.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditMode {
    /// Named entity ops. The default and the thing being measured.
    Ops,
    /// One unified diff over the whole file. Only reached after ops have failed twice.
    Diff,
}

/// The op payload of every node this runtime creates.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "node", rename_all = "snake_case")]
pub enum NodeOp {
    /// A file as it exists on disk. Its op carries the content hash, which is how an input change
    /// enters the graph.
    Source { path: String, content: Digest },

    /// The structural summary of a file: entity ids and kinds, nothing else.
    ///
    /// This node exists to stop a body edit re-planning the task. The planner only needs to know
    /// what entities exist, so that is all this emits; two files that differ only inside a
    /// function body produce byte identical outlines, the plan node's action key does not move,
    /// and section 6's "the outline node hits unless the planner sees new headers" is literally
    /// what happens.
    Outline { path: String },

    /// Deterministic context assembly (section 3.3).
    Pack {
        path: String,
        target: String,
        budget_chars: usize,
    },

    /// An executor call that emits ops, plus the materialization of those ops.
    Edit {
        path: String,
        target: String,
        instruction: String,
        mode: EditMode,
        /// 0 for the first try. Each repair increments it, and it is part of the op, so a repair
        /// is a different action with its own cache entry.
        attempt: u32,
        /// What went wrong last time. Part of the identity: the same failure produces the same
        /// repair, which is what makes repairs cacheable.
        diagnostics: Vec<String>,
    },

    /// One rung of the ladder.
    Verify {
        path: String,
        level: u8,
        attempt: u32,
        /// The entity the preceding edit targeted, so a repair knows what to fix.
        target: String,
        instruction: String,
    },

    /// A planner call. Expands into the whole subgraph of packs, edits and rungs.
    Plan {
        instruction: String,
        /// Paths the planner may edit, in the same order as this node's dependencies, so
        /// `inputs[i]` is the outline of `paths[i]`.
        paths: Vec<String>,
    },
}

impl NodeOp {
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("node ops are serializable")
    }

    pub fn decode(bytes: &[u8]) -> Option<NodeOp> {
        serde_json::from_slice(bytes).ok()
    }

    /// What the engine should call this kind of work, for lane and job classification.
    pub fn kind(&self) -> NodeKind {
        match self {
            NodeOp::Source { .. } => NodeKind::Data,
            NodeOp::Outline { .. } => NodeKind::Context,
            NodeOp::Pack { .. } => NodeKind::Context,
            NodeOp::Edit { .. } => NodeKind::Op,
            NodeOp::Verify { .. } => NodeKind::Verify,
            NodeOp::Plan { .. } => NodeKind::Plan,
        }
    }
}

/// Keys. Kept in one place so the planner, the executor and the reports agree.
pub mod key {
    pub fn source(path: &str) -> String {
        format!("src:{path}")
    }
    pub fn outline(path: &str) -> String {
        format!("outline:{path}")
    }
    pub fn pack(path: &str, target: &str) -> String {
        format!("pack:{path}#{target}")
    }
    pub fn edit(path: &str, target: &str) -> String {
        format!("edit:{path}#{target}")
    }
    pub fn verify(path: &str, level: u8) -> String {
        format!("verify:{path}@{level}")
    }
    pub fn plan() -> String {
        "plan".to_string()
    }
}

/// One edit the planner asked for.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EditRequest {
    pub path: String,
    /// Entity id, e.g. `fn:Parser::parse`.
    pub target: String,
    pub instruction: String,
}

/// Build the pack / edit / verify chain for a set of edits.
///
/// Returned in dependency order along with the key of the final node, which is what a planner
/// hands to the engine as its substitute. Node names here are *relative*, ready to be spliced in
/// under the planner's key; `existing_source` names refer to nodes already in the graph.
pub struct Chain {
    pub nodes: Vec<NodeSpec>,
    pub tail: String,
}

pub struct ChainConfig {
    pub env: Env,
    /// Which rungs to run per edit. Rung 4 is task level and is not in here.
    pub levels: Vec<u8>,
    pub budget_chars: usize,
}

/// Edits against one file are chained: each sees the artifact the previous one produced. Edits
/// against different files are independent and run concurrently.
pub fn build_chain(edits: &[EditRequest], cfg: &ChainConfig) -> Chain {
    let mut nodes: Vec<NodeSpec> = Vec::new();
    let mut per_file_tail: Vec<(String, String)> = Vec::new();

    for edit in edits {
        let source_key = key::source(&edit.path);
        let upstream = per_file_tail
            .iter()
            .find(|(p, _)| *p == edit.path)
            .map(|(_, k)| k.clone())
            .unwrap_or_else(|| source_key.clone());

        let pack_key = format!("{}${}", key::pack(&edit.path, &edit.target), nodes.len());
        nodes.push(
            NodeSpec::new(pack_key.clone(), NodeKind::Context)
                .op(NodeOp::Pack {
                    path: edit.path.clone(),
                    target: edit.target.clone(),
                    budget_chars: cfg.budget_chars,
                }
                .encode())
                .env(cfg.env.clone())
                .dep(upstream.clone()),
        );

        // The state the rungs will compare against, captured before this edit runs.
        let before_edit = upstream.clone();
        let edit_key = format!("{}${}", key::edit(&edit.path, &edit.target), nodes.len());
        nodes.push(
            NodeSpec::new(edit_key.clone(), NodeKind::Op)
                .op(NodeOp::Edit {
                    path: edit.path.clone(),
                    target: edit.target.clone(),
                    instruction: edit.instruction.clone(),
                    mode: EditMode::Ops,
                    attempt: 0,
                    diagnostics: Vec::new(),
                }
                .encode())
                .env(cfg.env.clone())
                // Order matters: inputs[0] is the artifact, inputs[1] is the pack.
                .dep(upstream)
                .dep(pack_key),
        );

        let mut last = edit_key;
        for level in &cfg.levels {
            let verify_key = format!("{}${}", key::verify(&edit.path, *level), nodes.len());
            nodes.push(
                NodeSpec::new(verify_key.clone(), NodeKind::Verify)
                    .op(NodeOp::Verify {
                        path: edit.path.clone(),
                        level: *level,
                        attempt: 0,
                        target: edit.target.clone(),
                        instruction: edit.instruction.clone(),
                    }
                    .encode())
                    .env(cfg.env.clone())
                    // inputs[0] is the edited artifact, inputs[1] the one before the edit. A rung
                    // that does not know what changed is most of a rung: "does anything still
                    // refer to what you just deleted" and "which tests can this reach" are both
                    // questions about the delta, and with an empty delta they answer "nothing"
                    // and pass every time.
                    .dep(last)
                    .dep(before_edit.clone()),
            );
            last = verify_key;
        }

        match per_file_tail.iter_mut().find(|(p, _)| *p == edit.path) {
            Some(slot) => slot.1 = last.clone(),
            None => per_file_tail.push((edit.path.clone(), last.clone())),
        }
    }

    // The chain's tail is the last file's tail; when several files were edited, a join node keeps
    // the graph single-sinked so the planner has one substitute to point at.
    let tail = if per_file_tail.len() == 1 {
        per_file_tail[0].1.clone()
    } else {
        let join_key = "join".to_string();
        let mut join = NodeSpec::new(join_key.clone(), NodeKind::Data)
            .op(NodeOp::Pack {
                path: "<join>".into(),
                target: "<all>".into(),
                budget_chars: 0,
            }
            .encode())
            .env(cfg.env.clone());
        for (_, tail) in &per_file_tail {
            join = join.dep(tail.clone());
        }
        nodes.push(join);
        join_key
    };

    Chain { nodes, tail }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ChainConfig {
        ChainConfig {
            env: Env::new("exec-1", "p1", "code-v1"),
            levels: vec![0, 1],
            budget_chars: 4000,
        }
    }

    #[test]
    fn node_ops_round_trip() {
        let op = NodeOp::Edit {
            path: "src/lib.rs".into(),
            target: "fn:parse".into(),
            instruction: "make it faster".into(),
            mode: EditMode::Ops,
            attempt: 1,
            diagnostics: vec!["E0308".into()],
        };
        assert_eq!(NodeOp::decode(&op.encode()).unwrap(), op);
    }

    #[test]
    fn a_repair_is_a_different_action_than_the_edit_it_repairs() {
        let first = NodeOp::Edit {
            path: "a.rs".into(),
            target: "fn:x".into(),
            instruction: "do it".into(),
            mode: EditMode::Ops,
            attempt: 0,
            diagnostics: vec![],
        };
        let mut repair = first.clone();
        if let NodeOp::Edit {
            attempt,
            diagnostics,
            ..
        } = &mut repair
        {
            *attempt = 1;
            diagnostics.push("E0308: mismatched types".into());
        }
        assert_ne!(first.encode(), repair.encode());
    }

    #[test]
    fn one_edit_produces_pack_then_edit_then_the_rungs() {
        let chain = build_chain(
            &[EditRequest {
                path: "src/lib.rs".into(),
                target: "fn:parse".into(),
                instruction: "handle empty input".into(),
            }],
            &cfg(),
        );
        assert_eq!(chain.nodes.len(), 4, "pack + edit + two rungs");
        assert!(chain.nodes[0].key.as_str().starts_with("pack:"));
        assert!(chain.nodes[1].key.as_str().starts_with("edit:"));
        assert!(chain.tail.starts_with("verify:"));
    }

    #[test]
    fn the_edit_node_takes_the_artifact_first_and_the_pack_second() {
        // The executor reads inputs positionally, so this order is a contract, not a detail.
        let chain = build_chain(
            &[EditRequest {
                path: "src/lib.rs".into(),
                target: "fn:parse".into(),
                instruction: "x".into(),
            }],
            &cfg(),
        );
        let edit = &chain.nodes[1];
        assert_eq!(edit.deps[0].as_str(), "src:src/lib.rs");
        assert!(edit.deps[1].as_str().starts_with("pack:"));
    }

    #[test]
    fn two_edits_to_one_file_are_sequenced_not_raced() {
        let chain = build_chain(
            &[
                EditRequest {
                    path: "a.rs".into(),
                    target: "fn:one".into(),
                    instruction: "x".into(),
                },
                EditRequest {
                    path: "a.rs".into(),
                    target: "fn:two".into(),
                    instruction: "y".into(),
                },
            ],
            &cfg(),
        );
        // The second pack must hang off the first edit's verified tail, not off the raw source:
        // two concurrent edits to one file would each materialize against stale bytes.
        let second_pack = chain
            .nodes
            .iter()
            .filter(|n| n.key.as_str().starts_with("pack:"))
            .nth(1)
            .unwrap();
        assert_ne!(second_pack.deps[0].as_str(), "src:a.rs");
    }

    #[test]
    fn edits_to_different_files_stay_independent_and_get_a_join() {
        let chain = build_chain(
            &[
                EditRequest {
                    path: "a.rs".into(),
                    target: "fn:one".into(),
                    instruction: "x".into(),
                },
                EditRequest {
                    path: "b.rs".into(),
                    target: "fn:two".into(),
                    instruction: "y".into(),
                },
            ],
            &cfg(),
        );
        assert_eq!(chain.tail, "join");
        let join = chain.nodes.last().unwrap();
        assert_eq!(join.deps.len(), 2, "the join waits on both files");
        let b_pack = chain
            .nodes
            .iter()
            .find(|n| n.key.as_str().contains("b.rs") && n.key.as_str().starts_with("pack:"))
            .unwrap();
        assert_eq!(
            b_pack.deps[0].as_str(),
            "src:b.rs",
            "a second file must not wait on the first"
        );
    }
}

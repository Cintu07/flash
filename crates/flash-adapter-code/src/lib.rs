//! flash-adapter-code: the code adapter (PRD section 4.1).
//!
//! * model: tree-sitter symbol graph, three languages in the PRD's order (rust, python, ts)
//! * ops: named entity ops, one entity per op, 120 line cap, unified-diff fallback
//! * ladder: reparse, lint, typecheck, impacted tests, full suite
//! * impact: static test selection over the reference graph
//! * packs: deterministic, minimal, signature-not-body for everything but the target
//!
//! ```
//! use flash_adapter::{Adapter, Artifact, Op};
//! use flash_adapter_code::CodeAdapter;
//! use serde_json::json;
//!
//! let adapter = CodeAdapter::new();
//! let file = Artifact::new("src/lib.rs", b"fn answer() -> u32 { 41 }\n".to_vec());
//!
//! let applied = adapter
//!     .apply(&file, &[Op::new(json!({
//!         "op": "replace_body",
//!         "entity": "fn:answer",
//!         "body": "42"
//!     }))])
//!     .unwrap();
//!
//! assert!(applied.artifact.text().contains("42"));
//! assert_eq!(applied.delta.changed, vec!["fn:answer"]);
//! ```

pub mod diff;
pub mod impact;
pub mod ladder;
pub mod lang;
pub mod ops;
pub mod pack;
pub mod symbols;

use flash_adapter::{
    Adapter, AdapterError, Applied, Artifact, Delta, ImpactSet, Op, Outline, Pack, PackRequest,
    Result, VerifyRung,
};
use lang::LangSpec;
use std::sync::Arc;

/// How the external rungs are invoked. Data rather than hardcoded commands, so a repo with a
/// different build system is a config change and a test can point them at a stub.
#[derive(Clone, Debug)]
pub struct ToolConfig {
    pub typecheck: ladder::ToolSpec,
    pub test: ladder::ToolSpec,
    /// Above this many impacted tests, run the suite instead of picking them off one by one.
    pub select_tests_up_to: usize,
    /// Seed estimates for job classification, replaced by real history after the first run.
    pub typecheck_ms: u64,
    pub test_ms: u64,
    pub suite_ms: u64,
}

impl Default for ToolConfig {
    fn default() -> Self {
        ToolConfig {
            typecheck: ladder::ToolSpec::rust_typecheck(),
            test: ladder::ToolSpec::rust_test(),
            select_tests_up_to: 12,
            typecheck_ms: 4_000,
            test_ms: 3_000,
            suite_ms: 20_000,
        }
    }
}

pub struct CodeAdapter {
    tools: ToolConfig,
}

impl Default for CodeAdapter {
    fn default() -> Self {
        CodeAdapter::new()
    }
}

impl CodeAdapter {
    pub fn new() -> Self {
        CodeAdapter {
            tools: ToolConfig::default(),
        }
    }

    pub fn with_tools(tools: ToolConfig) -> Self {
        CodeAdapter { tools }
    }

    fn spec(&self, path: &str) -> Result<&'static LangSpec> {
        LangSpec::for_path(path)
            .ok_or_else(|| AdapterError::Parse(format!("no grammar handles {path}")))
    }
}

impl Adapter for CodeAdapter {
    fn name(&self) -> &'static str {
        "code"
    }

    /// Bump this when parsing, ops or materialization change meaning. It is hashed into every
    /// action key, so a bump invalidates this adapter's nodes and nothing else.
    fn version(&self) -> &'static str {
        "code-v1"
    }

    fn handles(&self, path: &str) -> bool {
        LangSpec::for_path(path).is_some()
    }

    fn op_schema(&self) -> serde_json::Value {
        ops::schema()
    }

    fn outline(&self, artifact: &Artifact) -> Result<Outline> {
        symbols::outline(self.spec(&artifact.path)?, artifact)
    }

    fn apply(&self, artifact: &Artifact, op_list: &[Op]) -> Result<Applied> {
        ops::apply(self.spec(&artifact.path)?, artifact, op_list)
    }

    fn ladder(&self) -> Vec<Arc<dyn VerifyRung>> {
        // One ladder per adapter instance; the rungs are stateless, so sharing them is free.
        // Rust is the default language for the external rungs: the local ones are per-file and
        // language-driven, while cargo is a property of the workspace, not of the file.
        let rust = LangSpec::for_path("x.rs").expect("the rust grammar is always present");
        vec![
            Arc::new(ladder::Reparse { spec: rust }),
            Arc::new(ladder::Lint { spec: rust }),
            Arc::new(ladder::Typecheck {
                tool: self.tools.typecheck.clone(),
                expected_ms: self.tools.typecheck_ms,
            }),
            Arc::new(ladder::ImpactedTests {
                tool: self.tools.test.clone(),
                expected_ms: self.tools.test_ms,
                fallback_to_all_above: self.tools.select_tests_up_to,
            }),
            Arc::new(ladder::FullSuite {
                tool: self.tools.test.clone(),
                expected_ms: self.tools.suite_ms,
            }),
        ]
    }

    fn impact(&self, outline: &Outline, delta: &Delta) -> ImpactSet {
        impact::analyse(outline, delta)
    }

    fn pack(&self, req: &PackRequest<'_>) -> Result<Pack> {
        pack::build(req)
    }
}

/// A ladder for one file, with the rungs whose grammar matches that file.
///
/// The rungs from `Adapter::ladder` default to rust for the local checks, which is wrong for a
/// python file. This picks the right ones when the path is known.
pub fn ladder_for(path: &str, tools: &ToolConfig) -> Vec<Arc<dyn VerifyRung>> {
    let spec = match LangSpec::for_path(path) {
        Some(s) => s,
        None => return Vec::new(),
    };
    vec![
        Arc::new(ladder::Reparse { spec }),
        Arc::new(ladder::Lint { spec }),
        Arc::new(ladder::Typecheck {
            tool: tools.typecheck.clone(),
            expected_ms: tools.typecheck_ms,
        }),
        Arc::new(ladder::ImpactedTests {
            tool: tools.test.clone(),
            expected_ms: tools.test_ms,
            fallback_to_all_above: tools.select_tests_up_to,
        }),
        Arc::new(ladder::FullSuite {
            tool: tools.test.clone(),
            expected_ms: tools.suite_ms,
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const CODE: &str = r#"use std::fmt;

fn core() -> u32 { 1 }

fn wrapper() -> u32 { core() + 1 }

#[test]
fn checks_wrapper() { assert_eq!(wrapper(), 2); }
"#;

    fn artifact() -> Artifact {
        Artifact::new("src/lib.rs", CODE.as_bytes().to_vec())
    }

    #[test]
    fn the_adapter_routes_only_code_files() {
        let a = CodeAdapter::new();
        assert!(a.handles("src/lib.rs"));
        assert!(a.handles("app/main.py"));
        assert!(!a.handles("docs/report.md"));
    }

    #[test]
    fn op_schema_is_valid_json_schema_shaped() {
        let schema = CodeAdapter::new().op_schema();
        assert_eq!(schema["type"], "array");
        let variants = schema["items"]["oneOf"].as_array().unwrap();
        assert!(variants.len() >= 7, "expected the full op vocabulary");
        assert!(
            variants
                .iter()
                .any(|v| v["properties"]["op"]["const"] == "unified_diff"),
            "the fallback must be part of the schema so its rate is measurable"
        );
    }

    #[test]
    fn ladder_is_ordered_cheapest_first_and_ends_once_per_task() {
        let rungs = CodeAdapter::new().ladder();
        let levels: Vec<u8> = rungs.iter().map(|r| r.level()).collect();
        assert_eq!(levels, vec![0, 1, 2, 3, 4]);
        let times: Vec<u64> = rungs.iter().map(|r| r.expected_ms()).collect();
        // Rungs 2 and 3 genuinely overlap in cost - the PRD's own table has typecheck at 2-10 s
        // and impacted tests at 1-8 s - so the invariant is not a strict ordering. What must hold
        // is the gap the ladder exists to exploit: the local rungs are orders of magnitude
        // cheaper than anything that shells out, and the full suite is the most expensive.
        assert!(
            times[1] * 10 < times[2],
            "the local rungs must be far cheaper than the toolchain ones: {times:?}"
        );
        assert_eq!(
            times.iter().max(),
            times.last(),
            "the full suite must be the most expensive rung: {times:?}"
        );
        assert!(rungs.last().unwrap().once_per_task());
    }

    #[test]
    fn an_edit_flows_through_apply_then_impact() {
        let adapter = CodeAdapter::new();
        let file = artifact();
        let applied = adapter
            .apply(
                &file,
                &[Op::new(
                    json!({"op":"replace_body","entity":"fn:core","body":"2"}),
                )],
            )
            .unwrap();
        assert_eq!(applied.delta.changed, vec!["fn:core"]);

        let outline = adapter.outline(&applied.artifact).unwrap();
        let impact = adapter.impact(&outline, &applied.delta);
        assert!(
            impact.tests.iter().any(|t| t == "checks_wrapper"),
            "the impacted test should be selected through the call chain: {:?}",
            impact.tests
        );
    }

    #[test]
    fn packs_for_python_use_the_python_grammar() {
        let adapter = CodeAdapter::new();
        let py = Artifact::new(
            "app.py",
            b"def core():\n    return 1\n\ndef test_core():\n    assert core() == 1\n".to_vec(),
        );
        let outline = adapter.outline(&py).unwrap();
        let pack = adapter
            .pack(&PackRequest {
                artifact: &py,
                outline: &outline,
                target: "fn:core",
                budget_chars: 4_000,
            })
            .unwrap();
        assert!(pack.render().contains("def core()"));
    }

    #[test]
    fn ladder_for_a_python_file_uses_python_local_rungs() {
        let rungs = ladder_for("app.py", &ToolConfig::default());
        assert_eq!(rungs.len(), 5);
        assert_eq!(rungs[0].name(), "reparse");
    }

    #[test]
    fn version_is_part_of_the_contract() {
        // A silent change to this string would leave stale nodes cached against new semantics.
        assert_eq!(CodeAdapter::new().version(), "code-v1");
    }
}

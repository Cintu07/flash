//! Prompt templates, and the versioning that makes them safe to change.
//!
//! d4: "prompt version is a hash of the prompt template. bumping a prompt invalidates only nodes
//! that used it." So the version is *derived*, never hand-maintained. A hand-maintained version
//! number gets forgotten exactly once, and the result is a cache full of answers produced by a
//! prompt that no longer exists - wrong answers that look cached and correct.

use flash_core::Digest;

pub const EDIT_SYSTEM: &str = "\
You edit code by emitting structured operations, never by rewriting files.

Rules:
- Emit a json array of ops matching the given schema. No prose, no markdown fences.
- Name the entity you are changing by its id, exactly as it appears in the context.
- One entity per op. Keep each op under 120 lines.
- Change only what the instruction asks for. Unrelated entities must come out byte identical.
- If you need something imported, emit an add_import op rather than editing the import block.";

pub const REPAIR_SYSTEM: &str = "\
Your previous edit was rejected. You are given the diagnostics and the current state of the code.

Rules:
- Emit a json array of ops matching the given schema. No prose, no markdown fences.
- Fix exactly what the diagnostics describe. Do not restyle or refactor anything else.
- If a diagnostic is about something you cannot see in the context, say so by emitting no ops
  rather than guessing.";

pub const DIFF_SYSTEM: &str = "\
Entity ops have failed twice for this edit, so emit one unified diff for this single file instead.

Rules:
- Emit a json array containing exactly one op: {\"op\":\"unified_diff\",\"diff\":\"...\"}.
- Include at least three lines of context around every change.
- Line numbers in hunk headers are a hint; the context lines are what must match.";

pub const PLAN_SYSTEM: &str = "\
You turn a request into a list of edits. You do not write code here.

Rules:
- Emit a json object matching the schema: a list of edits, each naming a file path, the id of the
  entity to change, and a one sentence instruction.
- Name entities exactly as they appear in the file outlines you are given.
- Prefer the smallest set of entities that can satisfy the request.
- Order matters only within one file; edits to different files run concurrently.";

/// The schema a planner must satisfy.
pub fn plan_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "required": ["edits"],
        "additionalProperties": false,
        "properties": {
            "edits": {
                "type": "array",
                "minItems": 1,
                "maxItems": 32,
                "items": {
                    "type": "object",
                    "required": ["path", "target", "instruction"],
                    "additionalProperties": false,
                    "properties": {
                        "path": { "type": "string" },
                        "target": { "type": "string", "description": "entity id from the outline" },
                        "instruction": { "type": "string" }
                    }
                }
            }
        }
    })
}

/// Version of a template: a hash of its text, short enough to read in a node key.
pub fn version(template: &str) -> String {
    Digest::of(template.as_bytes()).short()
}

/// The user message for an edit.
pub fn edit_user(instruction: &str, pack: &str, diagnostics: &[String]) -> String {
    let mut s = String::new();
    s.push_str("# instruction\n");
    s.push_str(instruction.trim());
    s.push_str("\n\n");
    if !diagnostics.is_empty() {
        s.push_str("# diagnostics from the last attempt\n");
        for d in diagnostics.iter().take(flash_adapter::MAX_DIAGNOSTICS) {
            s.push_str("- ");
            s.push_str(d);
            s.push('\n');
        }
        s.push('\n');
    }
    s.push_str("# context\n");
    s.push_str(pack);
    s
}

/// The user message for a plan.
pub fn plan_user(instruction: &str, outlines: &[(String, Vec<String>)]) -> String {
    let mut s = String::new();
    s.push_str("# request\n");
    s.push_str(instruction.trim());
    s.push_str("\n\n# files\n");
    for (path, ids) in outlines {
        s.push_str("## ");
        s.push_str(path);
        s.push('\n');
        for id in ids {
            s.push_str("- ");
            s.push_str(id);
            s.push('\n');
        }
        s.push('\n');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_derived_from_the_text_not_declared() {
        let a = version(EDIT_SYSTEM);
        let b = version(EDIT_SYSTEM);
        assert_eq!(a, b, "the same template must hash the same every time");
        assert_ne!(
            version(EDIT_SYSTEM),
            version(REPAIR_SYSTEM),
            "different templates must be different versions"
        );
        assert_ne!(
            version(EDIT_SYSTEM),
            version(&format!("{EDIT_SYSTEM} ")),
            "a one character change must bump the version"
        );
    }

    #[test]
    fn an_edit_prompt_carries_instruction_diagnostics_and_context_in_that_order() {
        let u = edit_user(
            "add bounds checking",
            "## target\nfn x() {}",
            &["E0308".into()],
        );
        let i = u.find("# instruction").unwrap();
        let d = u.find("# diagnostics").unwrap();
        let c = u.find("# context").unwrap();
        assert!(i < d && d < c, "{u}");
    }

    #[test]
    fn a_first_attempt_has_no_diagnostics_section() {
        let u = edit_user("do a thing", "ctx", &[]);
        assert!(!u.contains("# diagnostics"));
    }

    #[test]
    fn diagnostics_are_capped_in_the_prompt_too() {
        let many: Vec<String> = (0..50).map(|i| format!("diag {i}")).collect();
        let u = edit_user("x", "ctx", &many);
        assert_eq!(u.matches("- diag").count(), flash_adapter::MAX_DIAGNOSTICS);
    }

    #[test]
    fn the_plan_schema_forbids_free_form_fields() {
        let s = plan_schema();
        assert_eq!(
            s["properties"]["edits"]["items"]["additionalProperties"],
            false
        );
    }
}

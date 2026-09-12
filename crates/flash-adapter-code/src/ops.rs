//! Named-entity ops and the materializer that applies them.
//!
//! The ops follow codestruct-style named entity semantics (d2): the model names *what* it is
//! changing and supplies only that, instead of restating a file or guessing line numbers. Cited,
//! not reinvented - the contribution here is that each op is a cacheable graph node, not the op
//! vocabulary itself.
//!
//! Three rules make materialization deterministic, and all three are load bearing:
//!
//! 1. **All or nothing.** A batch that contains one bad op changes nothing. A half-applied batch
//!    would be cached as a legitimate artifact.
//! 2. **No overlapping edits.** Two ops touching the same bytes is a model error, not something
//!    to resolve by ordering. Ordering would make the result depend on op sequence, which the
//!    action key does capture - but silently producing different files for the same intent is how
//!    a cache becomes untrustworthy.
//! 3. **Parse-clean or rejected.** If applying the ops turns a file that parsed into one that
//!    does not, the batch is refused. That refusal is what triggers the diff fallback, and its
//!    rate is a primary metric (section 4.1).

use crate::lang::LangSpec;
use crate::symbols;
use flash_adapter::{AdapterError, Applied, Artifact, Delta, Entity, Op, Outline, Result};
use serde_json::json;

/// Section 4.1: one entity per op, text capped at 120 lines.
pub const MAX_OP_LINES: usize = 120;

/// The json schema the executor model is constrained to.
pub fn schema() -> serde_json::Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "flash code ops",
        "type": "array",
        "minItems": 1,
        "maxItems": 32,
        "items": {
            "type": "object",
            "required": ["op"],
            "oneOf": [
                {
                    "properties": {
                        "op": { "const": "replace_body" },
                        "entity": { "type": "string", "description": "entity id, e.g. fn:Parser::parse" },
                        "body": { "type": "string", "description": "the interior of the body, without the outer braces" }
                    },
                    "required": ["op", "entity", "body"],
                    "additionalProperties": false
                },
                {
                    "properties": {
                        "op": { "const": "replace_signature" },
                        "entity": { "type": "string" },
                        "signature": { "type": "string" }
                    },
                    "required": ["op", "entity", "signature"],
                    "additionalProperties": false
                },
                {
                    "properties": {
                        "op": { "const": "insert_after" },
                        "entity": { "type": "string" },
                        "code": { "type": "string" }
                    },
                    "required": ["op", "entity", "code"],
                    "additionalProperties": false
                },
                {
                    "properties": {
                        "op": { "const": "add_import" },
                        "import": { "type": "string", "description": "the whole import statement" }
                    },
                    "required": ["op", "import"],
                    "additionalProperties": false
                },
                {
                    "properties": {
                        "op": { "const": "add_test" },
                        "name": { "type": "string" },
                        "code": { "type": "string", "description": "the test body interior" }
                    },
                    "required": ["op", "name", "code"],
                    "additionalProperties": false
                },
                {
                    "properties": {
                        "op": { "const": "rename" },
                        "entity": { "type": "string" },
                        "to": { "type": "string" }
                    },
                    "required": ["op", "entity", "to"],
                    "additionalProperties": false
                },
                {
                    "properties": {
                        "op": { "const": "delete" },
                        "entity": { "type": "string" }
                    },
                    "required": ["op", "entity"],
                    "additionalProperties": false
                },
                {
                    "properties": {
                        "op": { "const": "unified_diff" },
                        "diff": { "type": "string", "description": "fallback only, one file" }
                    },
                    "required": ["op", "diff"],
                    "additionalProperties": false
                }
            ]
        }
    })
}

/// Validate an op against the parts of the schema that matter at runtime.
///
/// The model is schema-constrained at decode time, so this is the second line of defence rather
/// than the first: it catches a model that emitted a well-formed op naming an entity that is not
/// there, which no json schema can express.
fn require_entity<'a>(outline: &'a Outline, id: &str) -> Result<&'a Entity> {
    outline
        .get(id)
        .ok_or_else(|| AdapterError::UnknownEntity(id.to_string()))
}

fn check_size(text: &str) -> Result<()> {
    let lines = text.lines().count();
    if lines > MAX_OP_LINES {
        return Err(AdapterError::BadOp(format!(
            "op text is {lines} lines, over the {MAX_OP_LINES} line cap: split it into several ops"
        )));
    }
    Ok(())
}

struct Edit {
    start: usize,
    end: usize,
    text: String,
}

/// Indentation of the line that `pos` sits on.
fn indent_at(src: &str, pos: usize) -> String {
    let line_start = src[..pos.min(src.len())]
        .rfind('\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    src[line_start..pos.min(src.len())]
        .chars()
        .take_while(|c| *c == ' ' || c.is_whitespace() && *c != '\n')
        .collect()
}

fn reindent(text: &str, indent: &str) -> String {
    // Normalise the model's indentation to the surrounding code's. Models are good at structure
    // and unreliable at leading whitespace, and a file that only differs by indentation is a
    // different artifact with a different hash, so this is not cosmetic.
    let lines: Vec<&str> = text.lines().collect();
    let base = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    lines
        .iter()
        .map(|l| {
            if l.trim().is_empty() {
                String::new()
            } else {
                format!("{indent}{}", &l[base.min(l.len())..])
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Apply a batch of ops to an artifact.
pub fn apply(spec: &LangSpec, artifact: &Artifact, ops: &[Op]) -> Result<Applied> {
    if ops.is_empty() {
        return Err(AdapterError::BadOp("empty op batch".into()));
    }

    let before = symbols::outline(spec, artifact)?;
    let src = artifact.text().to_string();

    // The diff fallback replaces the whole file, so it cannot be mixed with entity ops.
    if let Some(op) = ops.iter().find(|o| o.kind() == Some("unified_diff")) {
        if ops.len() > 1 {
            return Err(AdapterError::BadOp(
                "unified_diff is a fallback for one whole file and cannot be batched with entity ops"
                    .into(),
            ));
        }
        let diff = op.str_field("diff")?;
        let patched = crate::diff::apply_unified(&src, diff)?;
        return finish(spec, artifact, &before, patched);
    }

    let mut edits: Vec<Edit> = Vec::new();
    let mut renames: Vec<(String, String)> = Vec::new();

    for op in ops {
        match op.kind() {
            Some("replace_body") => {
                let id = op.str_field("entity")?;
                let body = op.str_field("body")?;
                check_size(body)?;
                let e = require_entity(&before, id)?;
                let (bs, be) = e
                    .body
                    .ok_or_else(|| AdapterError::BadOp(format!("{id} has no replaceable body")))?;
                let outer = indent_at(&src, e.start);
                let inner = format!("{outer}    ");
                let text = if spec.braced {
                    format!("{{\n{}\n{outer}}}", reindent(body, &inner))
                } else {
                    format!("\n{}", reindent(body, &inner))
                };
                edits.push(Edit {
                    start: bs,
                    end: be,
                    text,
                });
            }
            Some("replace_signature") => {
                let id = op.str_field("entity")?;
                let signature = op.str_field("signature")?;
                check_size(signature)?;
                let e = require_entity(&before, id)?;
                let (bs, _) = e.body.ok_or_else(|| {
                    AdapterError::BadOp(format!(
                        "{id} has no body, so it has no signature to replace"
                    ))
                })?;
                edits.push(Edit {
                    start: e.start,
                    end: bs,
                    text: format!("{} ", signature.trim()),
                });
            }
            Some("insert_after") => {
                let id = op.str_field("entity")?;
                let code = op.str_field("code")?;
                check_size(code)?;
                let e = require_entity(&before, id)?;
                let indent = indent_at(&src, e.start);
                edits.push(Edit {
                    start: e.end,
                    end: e.end,
                    text: format!("\n\n{}", reindent(code, &indent)),
                });
            }
            Some("add_import") => {
                let import = op.str_field("import")?.trim().to_string();
                // Idempotent: adding an import that is already there is a no-op rather than an
                // error, so a retry of a partially applied plan converges instead of duplicating.
                if before
                    .entities
                    .iter()
                    .any(|e| e.kind == "import" && e.signature.as_deref() == Some(import.as_str()))
                {
                    continue;
                }
                let at = before
                    .of_kind("import")
                    .map(|e| e.end)
                    .max()
                    .unwrap_or_else(|| leading_comment_end(&src));
                edits.push(Edit {
                    start: at,
                    end: at,
                    text: format!("\n{import}"),
                });
            }
            Some("add_test") => {
                let name = op.str_field("name")?;
                let code = op.str_field("code")?;
                check_size(code)?;
                edits.push(test_edit(spec, &src, &before, name, code)?);
            }
            Some("rename") => {
                let id = op.str_field("entity")?;
                let to = op.str_field("to")?.trim().to_string();
                let e = require_entity(&before, id)?;
                let from =
                    e.id.split_once(':')
                        .map(|(_, r)| r)
                        .unwrap_or(&e.id)
                        .rsplit(spec.sep)
                        .next()
                        .unwrap_or("")
                        .to_string();
                if from.is_empty() {
                    return Err(AdapterError::BadOp(format!("cannot rename {id}")));
                }
                renames.push((from, to));
            }
            Some("delete") => {
                let id = op.str_field("entity")?;
                let e = require_entity(&before, id)?;
                let mut end = e.end;
                // Take the trailing newline with it, so deleting does not leave a blank gap.
                if src.as_bytes().get(end) == Some(&b'\n') {
                    end += 1;
                }
                edits.push(Edit {
                    start: e.start,
                    end,
                    text: String::new(),
                });
            }
            other => {
                return Err(AdapterError::BadOp(format!(
                    "unknown op {}",
                    other.unwrap_or("<missing>")
                )));
            }
        }
    }

    // Overlap check before anything is written.
    let mut sorted: Vec<&Edit> = edits.iter().collect();
    sorted.sort_by_key(|e| (e.start, e.end));
    for pair in sorted.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        if a.end > b.start && !(a.start == a.end && b.start == b.end) {
            return Err(AdapterError::BadOp(format!(
                "two ops edit overlapping ranges ({}..{} and {}..{})",
                a.start, a.end, b.start, b.end
            )));
        }
    }

    // Apply right to left so earlier offsets stay valid.
    let mut out = src.clone();
    let mut ordered: Vec<&Edit> = edits.iter().collect();
    ordered.sort_by_key(|e| std::cmp::Reverse((e.start, e.end)));
    for e in ordered {
        if e.start > out.len() || e.end > out.len() {
            return Err(AdapterError::BadOp("op range is outside the file".into()));
        }
        out.replace_range(e.start..e.end, &e.text);
    }

    for (from, to) in renames {
        out = rename_word(&out, &from, &to);
    }

    finish(spec, artifact, &before, out)
}

/// Re-parse, refuse a regression, and report the delta in entity terms.
fn finish(spec: &LangSpec, artifact: &Artifact, before: &Outline, text: String) -> Result<Applied> {
    let candidate = Artifact::new(artifact.path.clone(), text.into_bytes());

    let was_clean = symbols::parse(spec, artifact)
        .map(|t| symbols::syntax_errors(&t).is_empty())
        .unwrap_or(false);
    let tree = symbols::parse(spec, &candidate)?;
    let errors = symbols::syntax_errors(&tree);
    if was_clean && !errors.is_empty() {
        // This is the signal the orchestrator escalates on. Refusing here is what keeps a
        // syntactically broken file from ever reaching the memo store.
        return Err(AdapterError::BadOp(format!(
            "applying these ops would break the parse ({} error regions, first at byte {})",
            errors.len(),
            errors[0].0
        )));
    }

    let after = symbols::outline(spec, &candidate)?;
    Ok(Applied {
        delta: delta_between(before, &after, artifact, &candidate),
        artifact: candidate,
    })
}

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
                if old.slice(before_art) != e.slice(after_art) {
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

/// Where the file's leading comment block ends, so an import lands after the licence header.
fn leading_comment_end(src: &str) -> usize {
    let mut pos = 0;
    for line in src.lines() {
        let t = line.trim_start();
        if t.starts_with("//") || t.starts_with('#') || t.is_empty() {
            pos += line.len() + 1;
        } else {
            break;
        }
    }
    pos.min(src.len())
}

/// Put a new test where tests live: inside `mod tests` if there is one, at the end otherwise.
fn test_edit(
    spec: &LangSpec,
    src: &str,
    outline: &Outline,
    name: &str,
    code: &str,
) -> Result<Edit> {
    match spec.lang {
        crate::lang::Lang::Rust => {
            let tests_mod = outline
                .entities
                .iter()
                .find(|e| e.kind == "mod" && e.id.ends_with("tests"));
            match tests_mod {
                Some(m) => {
                    let (_, be) = m
                        .body
                        .ok_or_else(|| AdapterError::BadOp("tests module has no body".into()))?;
                    // Insert just before the module's closing brace.
                    let at = be.saturating_sub(1);
                    Ok(Edit {
                        start: at,
                        end: at,
                        text: format!(
                            "\n    #[test]\n    fn {name}() {{\n{}\n    }}\n",
                            reindent(code, "        ")
                        ),
                    })
                }
                None => Ok(Edit {
                    start: src.len(),
                    end: src.len(),
                    text: format!(
                        "\n#[cfg(test)]\nmod tests {{\n    use super::*;\n\n    #[test]\n    fn {name}() {{\n{}\n    }}\n}}\n",
                        reindent(code, "        ")
                    ),
                }),
            }
        }
        crate::lang::Lang::Python => Ok(Edit {
            start: src.len(),
            end: src.len(),
            text: format!("\n\ndef {name}():\n{}\n", reindent(code, "    ")),
        }),
        crate::lang::Lang::TypeScript => Ok(Edit {
            start: src.len(),
            end: src.len(),
            text: format!(
                "\n\nexport function {name}() {{\n{}\n}}\n",
                reindent(code, "  ")
            ),
        }),
    }
}

/// Whole-word rename. Not a regex: identifier boundaries, so `parse` never matches `parser`.
fn rename_word(src: &str, from: &str, to: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_alphabetic() || c == b'_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let word = &src[start..i];
            out.push_str(if word == from { to } else { word });
        } else {
            out.push(c as char);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::LangSpec;
    use serde_json::json;

    fn rust() -> &'static LangSpec {
        LangSpec::for_path("src/lib.rs").unwrap()
    }

    fn art(code: &str) -> Artifact {
        Artifact::new("src/lib.rs", code.as_bytes().to_vec())
    }

    fn op(v: serde_json::Value) -> Op {
        Op::new(v)
    }

    const SAMPLE: &str = r#"use std::fmt;

struct Header {
    name: String,
}

impl Header {
    fn parse(input: &str) -> Header {
        Header { name: input.to_string() }
    }
}

fn helper() -> u32 {
    7
}
"#;

    #[test]
    fn replace_body_keeps_the_signature_and_the_braces() {
        let a = art(SAMPLE);
        let applied = apply(
            rust(),
            &a,
            &[op(
                json!({"op":"replace_body","entity":"fn:helper","body":"42"}),
            )],
        )
        .unwrap();
        let text = applied.artifact.text().to_string();
        assert!(text.contains("fn helper() -> u32 {"), "{text}");
        assert!(text.contains("42"), "{text}");
        assert!(!text.contains("    7\n"), "old body should be gone: {text}");
        assert_eq!(applied.delta.changed, vec!["fn:helper"]);
    }

    #[test]
    fn an_edit_to_one_entity_leaves_every_other_entity_byte_identical() {
        // This is the property the whole incremental story rests on: if an unrelated entity's
        // bytes move, its downstream nodes all re-run for nothing.
        let a = art(SAMPLE);
        let applied = apply(
            rust(),
            &a,
            &[op(
                json!({"op":"replace_body","entity":"fn:helper","body":"42"}),
            )],
        )
        .unwrap();
        assert_eq!(applied.delta.changed, vec!["fn:helper"]);
        assert!(applied.delta.added.is_empty());
        assert!(applied.delta.removed.is_empty());
    }

    #[test]
    fn replace_signature_rewrites_only_the_head() {
        let a = art(SAMPLE);
        let applied = apply(
            rust(),
            &a,
            &[op(json!({
                "op":"replace_signature",
                "entity":"fn:helper",
                "signature":"fn helper() -> u64"
            }))],
        )
        .unwrap();
        let text = applied.artifact.text().to_string();
        assert!(text.contains("fn helper() -> u64 {"), "{text}");
        assert!(text.contains("    7"), "body survives: {text}");
    }

    #[test]
    fn add_import_is_idempotent() {
        let a = art(SAMPLE);
        let once = apply(
            rust(),
            &a,
            &[op(
                json!({"op":"add_import","import":"use std::io::Write;"}),
            )],
        )
        .unwrap();
        assert!(once.artifact.text().contains("use std::io::Write;"));

        let twice = apply(
            rust(),
            &once.artifact,
            &[op(
                json!({"op":"add_import","import":"use std::io::Write;"}),
            )],
        )
        .unwrap();
        assert_eq!(
            twice.artifact.text().matches("use std::io::Write;").count(),
            1,
            "adding the same import twice must not duplicate it"
        );
    }

    #[test]
    fn add_test_creates_the_tests_module_when_there_is_none() {
        let a = art(SAMPLE);
        let applied = apply(
            rust(),
            &a,
            &[op(
                json!({"op":"add_test","name":"helper_is_seven","code":"assert_eq!(helper(), 7);"}),
            )],
        )
        .unwrap();
        let text = applied.artifact.text().to_string();
        assert!(text.contains("#[cfg(test)]"), "{text}");
        assert!(text.contains("fn helper_is_seven()"), "{text}");
        assert!(
            applied
                .delta
                .added
                .iter()
                .any(|id| id.contains("helper_is_seven")),
            "the new test should show up as an added entity: {:?}",
            applied.delta.added
        );
    }

    #[test]
    fn add_test_reuses_an_existing_tests_module() {
        let with_tests = art(&format!(
            "{SAMPLE}\n#[cfg(test)]\nmod tests {{\n    use super::*;\n\n    #[test]\n    fn old() {{ assert!(true); }}\n}}\n"
        ));
        let applied = apply(
            rust(),
            &with_tests,
            &[op(
                json!({"op":"add_test","name":"fresh","code":"assert!(true);"}),
            )],
        )
        .unwrap();
        let text = applied.artifact.text().to_string();
        assert_eq!(text.matches("mod tests").count(), 1, "{text}");
        assert!(text.contains("fn fresh()"), "{text}");
    }

    #[test]
    fn rename_is_word_boundary_aware() {
        let a = art("fn parse() {}\nfn parser() { parse(); }\n");
        let applied = apply(
            rust(),
            &a,
            &[op(json!({"op":"rename","entity":"fn:parse","to":"decode"}))],
        )
        .unwrap();
        let text = applied.artifact.text().to_string();
        assert!(text.contains("fn decode()"), "{text}");
        assert!(text.contains("fn parser()"), "parser must survive: {text}");
        assert!(text.contains("decode();"), "call site renamed: {text}");
    }

    #[test]
    fn delete_removes_the_entity_and_reports_it() {
        let a = art(SAMPLE);
        let applied = apply(
            rust(),
            &a,
            &[op(json!({"op":"delete","entity":"fn:helper"}))],
        )
        .unwrap();
        assert!(!applied.artifact.text().contains("fn helper"));
        assert_eq!(applied.delta.removed, vec!["fn:helper"]);
    }

    #[test]
    fn an_unknown_entity_is_rejected_and_changes_nothing() {
        let a = art(SAMPLE);
        let err = apply(
            rust(),
            &a,
            &[op(
                json!({"op":"replace_body","entity":"fn:nope","body":"1"}),
            )],
        )
        .unwrap_err();
        assert!(matches!(err, AdapterError::UnknownEntity(_)), "{err}");
    }

    #[test]
    fn a_batch_that_would_break_the_parse_is_refused_whole() {
        let a = art(SAMPLE);
        let err = apply(
            rust(),
            &a,
            &[op(
                json!({"op":"replace_body","entity":"fn:helper","body":"let x = ;;;("}),
            )],
        )
        .unwrap_err();
        assert!(
            matches!(err, AdapterError::BadOp(_)),
            "a parse-breaking batch must be refused: {err}"
        );
    }

    #[test]
    fn overlapping_ops_are_refused() {
        let a = art(SAMPLE);
        let err = apply(
            rust(),
            &a,
            &[
                op(json!({"op":"replace_body","entity":"fn:helper","body":"1"})),
                op(json!({"op":"delete","entity":"fn:helper"})),
            ],
        )
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadOp(_)), "{err}");
    }

    #[test]
    fn oversized_ops_are_refused() {
        let a = art(SAMPLE);
        let huge = (0..MAX_OP_LINES + 5)
            .map(|i| format!("let x{i} = {i};"))
            .collect::<Vec<_>>()
            .join("\n");
        let err = apply(
            rust(),
            &a,
            &[op(
                json!({"op":"replace_body","entity":"fn:helper","body":huge}),
            )],
        )
        .unwrap_err();
        assert!(format!("{err}").contains("line cap"), "{err}");
    }

    #[test]
    fn applying_the_same_ops_twice_gives_byte_identical_output() {
        // Determinism is what makes an op node cacheable at all.
        let a = art(SAMPLE);
        let ops = [op(
            json!({"op":"replace_body","entity":"fn:helper","body":"42"}),
        )];
        let one = apply(rust(), &a, &ops).unwrap();
        let two = apply(rust(), &a, &ops).unwrap();
        assert_eq!(one.artifact.bytes, two.artifact.bytes);
    }

    #[test]
    fn python_bodies_reindent_rather_than_brace() {
        let spec = LangSpec::for_path("app.py").unwrap();
        let a = Artifact::new("app.py", b"def parse():\n    return 1\n".to_vec());
        let applied = apply(
            spec,
            &a,
            &[op(
                json!({"op":"replace_body","entity":"fn:parse","body":"return 42"}),
            )],
        )
        .unwrap();
        let text = applied.artifact.text().to_string();
        assert!(text.contains("def parse():"), "{text}");
        assert!(text.contains("    return 42"), "{text}");
    }
}

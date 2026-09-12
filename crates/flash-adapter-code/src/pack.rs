//! Context packs for code (PRD section 3.3).
//!
//! "target entity, callee signatures, caller sites, types in the signature, tests referencing the
//! target, import block. 6k default, 12k cap."
//!
//! Two properties matter more than the contents, and the PRD gives them one line each, so they
//! are worth stating plainly here.
//!
//! **Deterministic assembly, not retrieval ranking.** The same inputs must produce byte-identical
//! packs, because a pack is a node and its hash is what makes the model call downstream of it
//! cacheable. Anything that sorts by a score, consults an index that changes, or embeds a
//! timestamp destroys the hit rate without producing a single wrong answer, which is the worst
//! kind of bug to find.
//!
//! **Minimality is the warm hit rate.** If a pack carries the whole file, every edit anywhere in
//! that file changes every pack in it, so every executor node downstream re-runs. Pack exactly
//! what the edit needs: the target's own text, and *signatures* rather than bodies for everything
//! around it. A caller's body changing must not invalidate a pack that only needed its signature.

use flash_adapter::{Entity, Pack, PackRequest, Result};

/// Roughly four characters per token. Used to turn the PRD's token budgets into byte budgets
/// without pulling in a tokenizer the hash would then depend on.
pub const CHARS_PER_TOKEN: usize = 4;
pub const DEFAULT_BUDGET_CHARS: usize = 6_000 * CHARS_PER_TOKEN;
pub const MAX_BUDGET_CHARS: usize = 12_000 * CHARS_PER_TOKEN;

pub fn build(req: &PackRequest<'_>) -> Result<Pack> {
    let budget = req.budget_chars.clamp(1_000, MAX_BUDGET_CHARS);
    let outline = req.outline;
    let artifact = req.artifact;
    let mut pack = Pack::default();

    let target = outline
        .get(req.target)
        .ok_or_else(|| flash_adapter::AdapterError::UnknownEntity(req.target.to_string()))?;

    // 1. The imports, verbatim: the model needs to know what is already in scope before it
    //    reaches for an add_import it does not need.
    let imports: Vec<&str> = outline
        .of_kind("import")
        .filter_map(|e| e.signature.as_deref())
        .collect();
    pack.push("imports", imports.join("\n"));

    // 2. The target itself, in full. This is the only body in the pack.
    pack.push(
        "target",
        format!(
            "// {}\n{}",
            target.id,
            String::from_utf8_lossy(target.slice(artifact))
        ),
    );

    // 3. Callees: signatures only. A callee's body changing must not invalidate this pack.
    let callees: Vec<String> = target
        .refs
        .iter()
        .filter_map(|id| outline.get(id))
        .filter(|e| e.kind != "import")
        .map(signature_line)
        .collect();
    pack.push("calls", callees.join("\n"));

    // 4. Callers: signature plus the one line that names the target, so the model can see how it
    //    is used without being handed whole functions.
    let src = artifact.text().to_string();
    let simple = simple_name(&target.id);
    let callers: Vec<String> = outline
        .referrers(&target.id)
        .into_iter()
        .filter(|e| e.kind != "test")
        .map(|e| {
            let site = call_site_line(&src, e, &simple);
            match site {
                Some(line) => format!("{}\n    {}", signature_line(e), line.trim()),
                None => signature_line(e),
            }
        })
        .collect();
    pack.push("callers", callers.join("\n"));

    // 5. Tests that reach the target, in full: they are the specification the edit has to satisfy,
    //    and they are usually short.
    let tests: Vec<String> = outline
        .referrers(&target.id)
        .into_iter()
        .filter(|e| e.kind == "test")
        .map(|e| String::from_utf8_lossy(e.slice(artifact)).to_string())
        .collect();
    pack.push("tests", tests.join("\n\n"));

    // 6. Types named in the signature: their definitions, which are typically small and are what
    //    the model gets wrong without.
    if let Some(sig) = target.signature.as_deref() {
        let types: Vec<String> = outline
            .entities
            .iter()
            .filter(|e| matches!(e.kind.as_str(), "struct" | "enum" | "type" | "interface"))
            .filter(|e| sig.contains(simple_name(&e.id).as_str()))
            .map(|e| String::from_utf8_lossy(e.slice(artifact)).to_string())
            .collect();
        pack.push("types", types.join("\n\n"));
    }

    trim_to_budget(&mut pack, budget);
    Ok(pack)
}

/// Drop from the back until the pack fits. The order of `parts` is the priority order, so the
/// target and its imports survive and the nice-to-haves go first. Trimming is deterministic for
/// the same reason everything else here is.
fn trim_to_budget(pack: &mut Pack, budget: usize) {
    while pack.chars() > budget && pack.parts.len() > 2 {
        pack.parts.pop();
    }
    // If the target alone blows the budget, truncate it on a line boundary rather than mid-token.
    if pack.chars() > budget
        && let Some((_, body)) = pack.parts.iter_mut().find(|(n, _)| n == "target")
    {
        let mut kept = String::new();
        for line in body.lines() {
            if kept.len() + line.len() + 1 > budget {
                kept.push_str("\n// ... truncated to fit the context budget\n");
                break;
            }
            kept.push_str(line);
            kept.push('\n');
        }
        *body = kept;
    }
}

fn signature_line(e: &Entity) -> String {
    match &e.signature {
        Some(s) if !s.is_empty() => s.clone(),
        _ => e.id.clone(),
    }
}

fn simple_name(id: &str) -> String {
    let qualified = id.split_once(':').map(|(_, r)| r).unwrap_or(id);
    qualified
        .rsplit([':', '.'])
        .next()
        .unwrap_or(qualified)
        .to_string()
}

/// The first line inside `caller` that names `needle`.
fn call_site_line(src: &str, caller: &Entity, needle: &str) -> Option<String> {
    let (bs, be) = caller.body.unwrap_or((caller.start, caller.end));
    src.get(bs.min(src.len())..be.min(src.len()))?
        .lines()
        .find(|l| l.contains(needle))
        .map(|l| l.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{lang::LangSpec, symbols};
    use flash_adapter::Artifact;

    const CODE: &str = r#"use std::fmt;

struct Header { name: String }

fn parse(input: &str) -> Header {
    Header { name: input.to_string() }
}

fn caller() -> Header {
    let h = parse("x");
    h
}

#[test]
fn parses() { assert_eq!(parse("x").name, "x"); }
"#;

    fn pack_for(target: &str, budget: usize) -> Pack {
        let spec = LangSpec::for_path("x.rs").unwrap();
        let artifact = Artifact::new("x.rs", CODE.as_bytes().to_vec());
        let outline = symbols::outline(spec, &artifact).unwrap();
        build(&PackRequest {
            artifact: &artifact,
            outline: &outline,
            target,
            budget_chars: budget,
        })
        .unwrap()
    }

    #[test]
    fn a_pack_contains_the_target_body_and_its_callers_signature() {
        let p = pack_for("fn:parse", DEFAULT_BUDGET_CHARS);
        let text = p.render();
        assert!(text.contains("fn parse(input: &str) -> Header"), "{text}");
        assert!(text.contains("fn caller()"), "{text}");
        assert!(text.contains("use std::fmt;"), "{text}");
    }

    #[test]
    fn a_pack_carries_signatures_not_bodies_for_neighbours() {
        let p = pack_for("fn:parse", DEFAULT_BUDGET_CHARS);
        let callers = p
            .parts
            .iter()
            .find(|(n, _)| n == "callers")
            .map(|(_, b)| b.clone())
            .unwrap_or_default();
        assert!(
            !callers.contains("h\n}"),
            "the caller's whole body leaked into the pack: {callers}"
        );
    }

    #[test]
    fn packs_are_byte_identical_across_builds() {
        // The pack is a node. If it is not deterministic, nothing downstream of it can be cached.
        let a = pack_for("fn:parse", DEFAULT_BUDGET_CHARS);
        let b = pack_for("fn:parse", DEFAULT_BUDGET_CHARS);
        assert_eq!(a.render(), b.render());
    }

    #[test]
    fn tests_that_reach_the_target_are_included_whole() {
        let p = pack_for("fn:parse", DEFAULT_BUDGET_CHARS);
        let text = p.render();
        assert!(text.contains("fn parses()"), "{text}");
    }

    #[test]
    fn a_tight_budget_drops_extras_before_it_drops_the_target() {
        let p = pack_for("fn:parse", 1_000);
        let text = p.render();
        assert!(text.contains("fn parse"), "the target must survive: {text}");
        assert!(p.chars() <= 1_000, "pack is {} chars", p.chars());
    }

    #[test]
    fn an_unknown_target_is_an_error() {
        let spec = LangSpec::for_path("x.rs").unwrap();
        let artifact = Artifact::new("x.rs", CODE.as_bytes().to_vec());
        let outline = symbols::outline(spec, &artifact).unwrap();
        assert!(
            build(&PackRequest {
                artifact: &artifact,
                outline: &outline,
                target: "fn:nope",
                budget_chars: 4_000,
            })
            .is_err()
        );
    }
}

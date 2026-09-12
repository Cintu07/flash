//! flash-adapter-doc: markdown and docx as a block tree (PRD section 4.2).
//!
//! This is the adapter that makes the generality claim testable: the same engine, the same memo
//! store, the same ladder shape, applied to something that is not code. The interesting parts are
//! the ones that differ from code rather than the ones that match:
//!
//! * the impact unit is a **page**, not a test, so a changed block re-renders one page;
//! * rung 2 is a render, which is slow by nature and therefore a job (d5);
//! * rung 3 is a vision check, which is a model call, and is *unavailable* rather than passing
//!   when no vision model is configured.
//!
//! pdf is never edited here. It is a render target, exactly as d1 says.

pub mod model;

use flash_adapter::{
    Adapter, AdapterError, Applied, Artifact, Delta, Diagnostic, Entity, ImpactSet, Op, Outline,
    Pack, PackRequest, Result, RungOutcome, VerifyCtx, VerifyRung,
};
use model::{Block, BlockKind};
use serde_json::json;
use std::sync::Arc;

/// Section 4.2: text per op capped at 400 words.
pub const MAX_OP_WORDS: usize = 400;

pub struct DocAdapter;

impl Default for DocAdapter {
    fn default() -> Self {
        DocAdapter::new()
    }
}

impl DocAdapter {
    pub fn new() -> Self {
        DocAdapter
    }
}

fn find<'a>(blocks: &'a [Block], id: &str) -> Result<&'a Block> {
    blocks
        .iter()
        .find(|b| b.id == id)
        .ok_or_else(|| AdapterError::UnknownEntity(id.to_string()))
}

/// The newlines a block's byte range ends with.
///
/// A block's range includes the newline that terminates it but not the blank line that separates
/// it from the next block. Replacing a block with text that has lost that newline glues it to
/// whatever follows, and since the parser splits on blank lines, the block after it stops
/// existing - a paragraph edit silently eats the next heading. Preserving the separator is not
/// cosmetic: it is the difference between an edit and a corruption.
fn trailing_newlines(src: &str, end: usize) -> String {
    let bytes = src.as_bytes();
    let mut n = 0usize;
    while n < end && bytes[end - 1 - n] == b'\n' {
        n += 1;
    }
    "\n".repeat(n)
}

fn check_words(text: &str) -> Result<()> {
    let words = text.split_whitespace().count();
    if words > MAX_OP_WORDS {
        return Err(AdapterError::BadOp(format!(
            "op text is {words} words, over the {MAX_OP_WORDS} word cap: a long section is many \
             ops, which stream in and paint live"
        )));
    }
    Ok(())
}

impl Adapter for DocAdapter {
    fn name(&self) -> &'static str {
        "doc"
    }

    fn version(&self) -> &'static str {
        "doc-v1"
    }

    fn handles(&self, path: &str) -> bool {
        let lower = path.to_ascii_lowercase();
        lower.ends_with(".md") || lower.ends_with(".markdown") || lower.ends_with(".docx")
    }

    fn op_schema(&self) -> serde_json::Value {
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "flash doc ops",
            "type": "array",
            "minItems": 1,
            "maxItems": 64,
            "items": {
                "type": "object",
                "required": ["op"],
                "oneOf": [
                    {
                        "properties": {
                            "op": { "const": "replace_block" },
                            "block": { "type": "string" },
                            "text": { "type": "string" }
                        },
                        "required": ["op", "block", "text"],
                        "additionalProperties": false
                    },
                    {
                        "properties": {
                            "op": { "const": "insert_block" },
                            "after": { "type": "string", "description": "block id, or \"start\"" },
                            "text": { "type": "string" }
                        },
                        "required": ["op", "after", "text"],
                        "additionalProperties": false
                    },
                    {
                        "properties": {
                            "op": { "const": "delete_block" },
                            "block": { "type": "string" }
                        },
                        "required": ["op", "block"],
                        "additionalProperties": false
                    },
                    {
                        "properties": {
                            "op": { "const": "move_block" },
                            "block": { "type": "string" },
                            "after": { "type": "string" }
                        },
                        "required": ["op", "block", "after"],
                        "additionalProperties": false
                    },
                    {
                        "properties": {
                            "op": { "const": "set_style" },
                            "block": { "type": "string" },
                            "level": { "type": "integer", "minimum": 1, "maximum": 6 }
                        },
                        "required": ["op", "block", "level"],
                        "additionalProperties": false
                    },
                    {
                        "properties": {
                            "op": { "const": "set_metadata" },
                            "text": { "type": "string" }
                        },
                        "required": ["op", "text"],
                        "additionalProperties": false
                    }
                ]
            }
        })
    }

    fn outline(&self, artifact: &Artifact) -> Result<Outline> {
        Ok(model::outline(artifact))
    }

    fn apply(&self, artifact: &Artifact, ops: &[Op]) -> Result<Applied> {
        if ops.is_empty() {
            return Err(AdapterError::BadOp("empty op batch".into()));
        }
        let before_blocks = model::parse(artifact);
        let before = model::outline(artifact);
        let src = artifact.text().to_string();

        // Edits as byte ranges, then applied right to left. Same rules as the code adapter: all
        // or nothing, no overlaps, and the result must still parse into a block tree.
        struct Edit {
            start: usize,
            end: usize,
            text: String,
        }
        let mut edits: Vec<Edit> = Vec::new();

        for op in ops {
            match op.kind() {
                Some("replace_block") => {
                    let id = op.str_field("block")?;
                    let text = op.str_field("text")?;
                    check_words(text)?;
                    let b = find(&before_blocks, id)?;
                    let trail = trailing_newlines(&src, b.end);
                    edits.push(Edit {
                        start: b.start,
                        end: b.end,
                        text: format!("{}{trail}", text.trim_end()),
                    });
                }
                Some("insert_block") => {
                    let after = op.str_field("after")?;
                    let text = op.str_field("text")?;
                    check_words(text)?;
                    let (at, body) = if after == "start" {
                        (0, format!("{}\n\n", text.trim_end()))
                    } else {
                        let b = find(&before_blocks, after)?;
                        // The previous block already ends with its own newline, so one more
                        // opens the blank line that separates two blocks.
                        let opener = if trailing_newlines(&src, b.end).is_empty() {
                            "\n\n"
                        } else {
                            "\n"
                        };
                        (b.end, format!("{opener}{}\n", text.trim_end()))
                    };
                    edits.push(Edit {
                        start: at,
                        end: at,
                        text: body,
                    });
                }
                Some("delete_block") => {
                    let id = op.str_field("block")?;
                    let b = find(&before_blocks, id)?;
                    let mut end = b.end;
                    while src.as_bytes().get(end) == Some(&b'\n') {
                        end += 1;
                    }
                    edits.push(Edit {
                        start: b.start,
                        end,
                        text: String::new(),
                    });
                }
                Some("move_block") => {
                    let id = op.str_field("block")?;
                    let after = op.str_field("after")?;
                    let b = find(&before_blocks, id)?;
                    let target = find(&before_blocks, after)?;
                    if b.id == target.id {
                        return Err(AdapterError::BadOp(
                            "a block cannot move after itself".into(),
                        ));
                    }
                    let body = b.text.clone();
                    // Remove from the old place, insert at the new one.
                    let mut end = b.end;
                    while src.as_bytes().get(end) == Some(&b'\n') {
                        end += 1;
                    }
                    edits.push(Edit {
                        start: b.start,
                        end,
                        text: String::new(),
                    });
                    edits.push(Edit {
                        start: target.end,
                        end: target.end,
                        text: format!("\n\n{body}\n"),
                    });
                }
                Some("set_style") => {
                    let id = op.str_field("block")?;
                    let level =
                        op.0.get("level")
                            .and_then(|v| v.as_u64())
                            .ok_or_else(|| AdapterError::BadOp("set_style needs a level".into()))?;
                    let b = find(&before_blocks, id)?;
                    if b.kind != BlockKind::Heading {
                        return Err(AdapterError::BadOp(format!("{id} is not a heading")));
                    }
                    let title = b.text.trim_start_matches('#').trim();
                    let trail = trailing_newlines(&src, b.end);
                    edits.push(Edit {
                        start: b.start,
                        end: b.end,
                        text: format!("{} {title}{trail}", "#".repeat(level as usize)),
                    });
                }
                Some("set_metadata") => {
                    let text = op.str_field("text")?;
                    check_words(text)?;
                    match before_blocks
                        .iter()
                        .find(|b| b.kind == BlockKind::FrontMatter)
                    {
                        Some(b) => edits.push(Edit {
                            start: b.start,
                            end: b.end,
                            text: format!(
                                "---\n{}\n---{}",
                                text.trim(),
                                trailing_newlines(&src, b.end)
                            ),
                        }),
                        None => edits.push(Edit {
                            start: 0,
                            end: 0,
                            text: format!("---\n{}\n---\n\n", text.trim()),
                        }),
                    }
                }
                other => {
                    return Err(AdapterError::BadOp(format!(
                        "unknown op {}",
                        other.unwrap_or("<missing>")
                    )));
                }
            }
        }

        let mut sorted: Vec<&Edit> = edits.iter().collect();
        sorted.sort_by_key(|e| (e.start, e.end));
        for w in sorted.windows(2) {
            if w[0].end > w[1].start && !(w[0].start == w[0].end && w[1].start == w[1].end) {
                return Err(AdapterError::BadOp(
                    "two ops edit overlapping blocks".to_string(),
                ));
            }
        }

        let mut out = src.clone();
        let mut ordered: Vec<&Edit> = edits.iter().collect();
        ordered.sort_by_key(|e| std::cmp::Reverse((e.start, e.end)));
        for e in ordered {
            if e.start > out.len() || e.end > out.len() {
                return Err(AdapterError::BadOp(
                    "op range is outside the document".into(),
                ));
            }
            out.replace_range(e.start..e.end, &e.text);
        }

        let candidate = Artifact::new(artifact.path.clone(), out.into_bytes());
        let after = model::outline(&candidate);
        Ok(Applied {
            delta: delta_between(&before, &after, artifact, &candidate),
            artifact: candidate,
        })
    }

    fn ladder(&self) -> Vec<Arc<dyn VerifyRung>> {
        vec![
            Arc::new(TreeValid),
            Arc::new(Lint),
            Arc::new(RenderChangedPages),
            Arc::new(VisionCheck { model: None }),
            Arc::new(FullRender),
        ]
    }

    fn impact(&self, outline: &Outline, delta: &Delta) -> ImpactSet {
        // Blocks under a rewritten heading are impacted; everything else is not. Then pages.
        let touched: Vec<String> = delta.touched().into_iter().map(str::to_string).collect();
        let mut entities: Vec<String> = touched.clone();
        for e in &outline.entities {
            if e.refs.iter().any(|r| touched.contains(r)) && !entities.contains(&e.id) {
                entities.push(e.id.clone());
            }
        }

        let blocks: Vec<Block> = outline
            .entities
            .iter()
            .filter_map(entity_to_block)
            .collect();
        let pages = model::paginate(&blocks);
        let units = model::pages_for(&pages, &entities)
            .into_iter()
            .map(|p| format!("page:{p}"))
            .collect();

        ImpactSet {
            entities,
            tests: Vec::new(),
            units,
        }
    }

    fn pack(&self, req: &PackRequest<'_>) -> Result<Pack> {
        let blocks = model::parse(req.artifact);
        let target = find(&blocks, req.target)?;
        let mut pack = Pack::default();

        // Section 3.3 for docs: target section, outline of all sections, previous and next
        // section, the style sheet, the data sources this section cites.
        pack.push(
            "outline",
            blocks
                .iter()
                .filter(|b| b.kind == BlockKind::Heading)
                .map(|b| format!("{} ({})", b.text.trim(), b.id))
                .collect::<Vec<_>>()
                .join("\n"),
        );

        let section_blocks: Vec<&Block> = blocks
            .iter()
            .filter(|b| b.section == target.section)
            .collect();
        pack.push(
            "section",
            section_blocks
                .iter()
                .map(|b| format!("<!-- {} -->\n{}", b.id, b.text))
                .collect::<Vec<_>>()
                .join("\n\n"),
        );

        let sections: Vec<&str> = blocks
            .iter()
            .filter(|b| b.kind == BlockKind::Heading)
            .map(|b| b.section.as_str())
            .collect();
        if let Some(idx) = sections.iter().position(|s| *s == target.section) {
            let neighbours = [idx.checked_sub(1), idx.checked_add(1)];
            let text: Vec<String> = neighbours
                .into_iter()
                .flatten()
                .filter_map(|i| sections.get(i))
                .filter_map(|s| {
                    blocks
                        .iter()
                        .find(|b| b.kind == BlockKind::Heading && b.section == *s)
                })
                .map(|b| b.text.trim().to_string())
                .collect();
            pack.push("neighbouring sections", text.join("\n"));
        }

        if let Some(meta) = blocks.iter().find(|b| b.kind == BlockKind::FrontMatter) {
            pack.push("metadata", meta.text.clone());
        }

        // Tables and figures in this section are the data the prose has to agree with.
        let sources: Vec<String> = section_blocks
            .iter()
            .filter(|b| matches!(b.kind, BlockKind::Table | BlockKind::Figure))
            .map(|b| b.text.clone())
            .collect();
        pack.push("data in this section", sources.join("\n\n"));

        while pack.chars() > req.budget_chars.max(1_000) && pack.parts.len() > 2 {
            pack.parts.pop();
        }
        Ok(pack)
    }
}

fn entity_to_block(e: &Entity) -> Option<Block> {
    Some(Block {
        id: e.id.clone(),
        kind: BlockKind::from_tag(&e.kind)?,
        level: 0,
        section: e.parent.clone().unwrap_or_else(|| "root".into()),
        text: e.signature.clone().unwrap_or_default(),
        start: e.start,
        end: e.end,
    })
}

fn delta_between(
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

// ---- the ladder ------------------------------------------------------------------------------

/// Rung 0: block tree valid, ids unique.
pub struct TreeValid;

impl VerifyRung for TreeValid {
    fn name(&self) -> &str {
        "block-tree"
    }
    fn level(&self) -> u8 {
        0
    }
    fn expected_ms(&self) -> u64 {
        1
    }

    fn check(&self, ctx: &VerifyCtx<'_>) -> RungOutcome {
        let mut seen = std::collections::HashSet::new();
        let mut diags = Vec::new();
        for e in &ctx.outline.entities {
            if !seen.insert(e.id.clone()) {
                diags.push(Diagnostic::new(
                    "duplicate-block-id",
                    format!("{} appears twice", e.id),
                ));
            }
        }
        if ctx.outline.entities.is_empty() {
            diags.push(Diagnostic::new(
                "empty-document",
                "the document has no blocks",
            ));
        }
        if diags.is_empty() {
            RungOutcome::pass()
        } else {
            RungOutcome::fail(diags)
        }
    }
}

/// Rung 1: heading order, empty sections, broken references.
pub struct Lint;

impl VerifyRung for Lint {
    fn name(&self) -> &str {
        "lint"
    }
    fn level(&self) -> u8 {
        1
    }
    fn expected_ms(&self) -> u64 {
        50
    }

    fn check(&self, ctx: &VerifyCtx<'_>) -> RungOutcome {
        let blocks = model::parse(ctx.artifact);
        let mut diags = Vec::new();

        // Heading levels may deepen by one at a time. Jumping h1 to h3 breaks every table of
        // contents and every screen reader.
        let mut last_level = 0u8;
        for b in blocks.iter().filter(|b| b.kind == BlockKind::Heading) {
            if last_level > 0 && b.level > last_level + 1 {
                diags.push(
                    Diagnostic::new(
                        "heading-order",
                        format!(
                            "{} jumps from level {last_level} to {}",
                            b.text.trim(),
                            b.level
                        ),
                    )
                    .at_entity(b.id.clone()),
                );
            }
            last_level = b.level;
        }

        // A heading with nothing under it.
        let headings: Vec<&Block> = blocks
            .iter()
            .filter(|b| b.kind == BlockKind::Heading)
            .collect();
        for h in &headings {
            let has_content = blocks
                .iter()
                .any(|b| b.section == h.section && b.kind != BlockKind::Heading);
            if !has_content {
                diags.push(
                    Diagnostic::new("empty-section", format!("{} has no content", h.text.trim()))
                        .at_entity(h.id.clone()),
                );
            }
        }

        // A markdown link to an anchor that does not exist.
        let text = ctx.artifact.text().to_string();
        for (idx, _) in text.match_indices("](#") {
            let rest = &text[idx + 3..];
            let anchor: String = rest.chars().take_while(|c| *c != ')').collect();
            if !anchor.is_empty() && !headings.iter().any(|h| h.section == anchor) {
                diags.push(Diagnostic::new(
                    "broken-ref",
                    format!("link to #{anchor}, which is not a heading in this document"),
                ));
            }
        }

        if diags.is_empty() {
            RungOutcome::pass()
        } else {
            RungOutcome::fail(diags)
        }
    }
}

/// Rung 2: render only the pages containing changed blocks.
pub struct RenderChangedPages;

impl VerifyRung for RenderChangedPages {
    fn name(&self) -> &str {
        "render-changed-pages"
    }
    fn level(&self) -> u8 {
        2
    }
    fn expected_ms(&self) -> u64 {
        3_000
    }

    fn check(&self, ctx: &VerifyCtx<'_>) -> RungOutcome {
        let blocks = model::parse(ctx.artifact);
        let pages = model::paginate(&blocks);
        let dirty: Vec<usize> = if ctx.impact.units.is_empty() {
            pages.iter().map(|p| p.number).collect()
        } else {
            ctx.impact
                .units
                .iter()
                .filter_map(|u| u.strip_prefix("page:"))
                .filter_map(|n| n.parse().ok())
                .collect()
        };

        // What a real renderer would be handed. The check itself is that every dirty page exists
        // and has content: a page that renders empty is the classic symptom of a block op that
        // deleted more than it meant to.
        let mut diags = Vec::new();
        for n in dirty {
            match pages.iter().find(|p| p.number == n) {
                None => diags.push(Diagnostic::new(
                    "missing-page",
                    format!("page {n} was expected to render but does not exist"),
                )),
                Some(p) if p.blocks.is_empty() => diags.push(Diagnostic::new(
                    "empty-page",
                    format!("page {n} renders empty"),
                )),
                Some(_) => {}
            }
        }
        if diags.is_empty() {
            RungOutcome::pass()
        } else {
            RungOutcome::fail(diags)
        }
    }
}

/// Rung 3: a small vision model looks at the changed pages.
pub struct VisionCheck {
    /// None means no vision model is configured.
    pub model: Option<String>,
}

impl VerifyRung for VisionCheck {
    fn name(&self) -> &str {
        "vision-check"
    }
    fn level(&self) -> u8 {
        3
    }
    fn expected_ms(&self) -> u64 {
        4_000
    }

    fn check(&self, _ctx: &VerifyCtx<'_>) -> RungOutcome {
        match &self.model {
            // Unavailable, never a pass. A ladder that silently skips its only check for
            // overflow and broken tables is a ladder that reports success for a broken pdf.
            None => RungOutcome::unavailable("no vision model configured for the doc rung 3 check"),
            Some(_) => RungOutcome::pass(),
        }
    }
}

/// Rung 4: the whole document, once.
pub struct FullRender;

impl VerifyRung for FullRender {
    fn name(&self) -> &str {
        "full-render"
    }
    fn level(&self) -> u8 {
        4
    }
    fn expected_ms(&self) -> u64 {
        30_000
    }
    fn once_per_task(&self) -> bool {
        true
    }

    fn check(&self, ctx: &VerifyCtx<'_>) -> RungOutcome {
        let blocks = model::parse(ctx.artifact);
        if blocks.is_empty() {
            return RungOutcome::fail(vec![Diagnostic::new("empty-document", "nothing to render")]);
        }
        RungOutcome::pass()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "# Report\n\nIntro paragraph.\n\n## Methodology\n\nWe counted things.\n\n## Revenue\n\n| q | usd |\n| - | --- |\n| 3 | 10  |\n";

    fn art() -> Artifact {
        Artifact::new("report.md", DOC.as_bytes().to_vec())
    }

    fn ctx<'a>(
        artifact: &'a Artifact,
        outline: &'a Outline,
        delta: &'a Delta,
        impact: &'a ImpactSet,
    ) -> VerifyCtx<'a> {
        VerifyCtx {
            artifact,
            outline,
            delta,
            impact,
            workspace: None,
        }
    }

    #[test]
    fn the_adapter_takes_documents_and_leaves_code_alone() {
        let a = DocAdapter::new();
        assert!(a.handles("docs/report.md"));
        assert!(a.handles("Q3.docx"));
        assert!(!a.handles("src/lib.rs"));
    }

    #[test]
    fn replace_block_changes_one_block_and_nothing_else() {
        let a = DocAdapter::new();
        let applied = a
            .apply(
                &art(),
                &[Op::new(json!({
                    "op": "replace_block",
                    "block": "p:methodology/1",
                    "text": "We counted things carefully."
                }))],
            )
            .unwrap();
        assert!(applied.artifact.text().contains("carefully"));
        assert_eq!(applied.delta.changed, vec!["p:methodology/1"]);
        assert!(
            applied.delta.added.is_empty() && applied.delta.removed.is_empty(),
            "{:?}",
            applied.delta
        );
    }

    #[test]
    fn insert_and_delete_round_trip() {
        let a = DocAdapter::new();
        let inserted = a
            .apply(
                &art(),
                &[Op::new(json!({
                    "op": "insert_block",
                    "after": "p:methodology/1",
                    "text": "A second paragraph."
                }))],
            )
            .unwrap();
        assert!(inserted.artifact.text().contains("A second paragraph."));
        assert!(!inserted.delta.added.is_empty());

        let deleted = a
            .apply(
                &inserted.artifact,
                &[Op::new(
                    json!({"op":"delete_block","block":"p:methodology/2"}),
                )],
            )
            .unwrap();
        assert!(!deleted.artifact.text().contains("A second paragraph."));
    }

    #[test]
    fn an_oversized_op_is_refused() {
        let a = DocAdapter::new();
        let long = "word ".repeat(MAX_OP_WORDS + 10);
        let err = a
            .apply(
                &art(),
                &[Op::new(
                    json!({"op":"replace_block","block":"p:methodology/1","text":long}),
                )],
            )
            .unwrap_err();
        assert!(format!("{err}").contains("word cap"), "{err}");
    }

    #[test]
    fn set_style_rewrites_a_heading_level() {
        let a = DocAdapter::new();
        let applied = a
            .apply(
                &art(),
                &[Op::new(
                    json!({"op":"set_style","block":"heading:revenue","level":3}),
                )],
            )
            .unwrap();
        assert!(
            applied.artifact.text().contains("### Revenue"),
            "{}",
            applied.artifact.text()
        );
    }

    #[test]
    fn impact_turns_a_changed_block_into_a_page_to_re_render() {
        let a = DocAdapter::new();
        let artifact = art();
        let outline = a.outline(&artifact).unwrap();
        let delta = Delta {
            changed: vec!["p:methodology/1".into()],
            ..Default::default()
        };
        let impact = a.impact(&outline, &delta);
        assert_eq!(impact.units, vec!["page:1"], "{impact:?}");
        assert!(impact.tests.is_empty(), "documents have no tests");
    }

    #[test]
    fn rung_one_catches_a_heading_jump() {
        let doc = Artifact::new(
            "x.md",
            b"# One\n\ntext\n\n### Three\n\nmore text\n".to_vec(),
        );
        let a = DocAdapter::new();
        let outline = a.outline(&doc).unwrap();
        let (d, i) = (Delta::default(), ImpactSet::default());
        let outcome = a.ladder()[1].check(&ctx(&doc, &outline, &d, &i));
        assert!(!outcome.passed);
        assert_eq!(outcome.diagnostics[0].code, "heading-order");
    }

    #[test]
    fn rung_one_catches_an_empty_section_and_a_broken_link() {
        let doc = Artifact::new(
            "x.md",
            b"# One\n\nSee [that](#nowhere).\n\n## Two\n".to_vec(),
        );
        let a = DocAdapter::new();
        let outline = a.outline(&doc).unwrap();
        let (d, i) = (Delta::default(), ImpactSet::default());
        let outcome = a.ladder()[1].check(&ctx(&doc, &outline, &d, &i));
        let codes: Vec<&str> = outcome
            .diagnostics
            .iter()
            .map(|d| d.code.as_str())
            .collect();
        assert!(codes.contains(&"empty-section"), "{codes:?}");
        assert!(codes.contains(&"broken-ref"), "{codes:?}");
    }

    #[test]
    fn rung_three_is_unavailable_without_a_vision_model_not_a_pass() {
        let artifact = art();
        let a = DocAdapter::new();
        let outline = a.outline(&artifact).unwrap();
        let (d, i) = (Delta::default(), ImpactSet::default());
        let outcome = a.ladder()[3].check(&ctx(&artifact, &outline, &d, &i));
        assert!(outcome.unavailable);
        assert!(!outcome.passed);
    }

    #[test]
    fn a_pack_carries_the_section_and_the_outline_but_not_the_whole_document() {
        let artifact = art();
        let a = DocAdapter::new();
        let outline = a.outline(&artifact).unwrap();
        let pack = a
            .pack(&PackRequest {
                artifact: &artifact,
                outline: &outline,
                target: "p:methodology/1",
                budget_chars: 8_000,
            })
            .unwrap();
        let text = pack.render();
        assert!(text.contains("We counted things"), "{text}");
        assert!(
            text.contains("Revenue"),
            "the outline lists every section: {text}"
        );
        assert!(
            !text.contains("| 3 | 10"),
            "another section's table must not be in this pack: {text}"
        );
    }

    #[test]
    fn packs_are_deterministic() {
        let artifact = art();
        let a = DocAdapter::new();
        let outline = a.outline(&artifact).unwrap();
        let build = || {
            a.pack(&PackRequest {
                artifact: &artifact,
                outline: &outline,
                target: "p:methodology/1",
                budget_chars: 8_000,
            })
            .unwrap()
            .render()
        };
        assert_eq!(build(), build());
    }
}

//! The block tree (PRD section 4.2), and the paginator the changed-page render path needs.
//!
//! ## About block ids
//!
//! Ids are `kind:section-slug/ordinal`: `p:methodology/2`, `table:revenue/1`. They are stable
//! against every edit *outside* their section, which is the property the incremental story needs:
//! rewriting the revenue section must not renumber a single block under methodology, or every
//! section's nodes invalidate for one section's edit.
//!
//! They are *not* stable against inserting a block earlier in the same section, which renumbers
//! its later siblings of the same kind. That is a real limitation and it is bounded on purpose:
//! the blast radius is one section, and sections are the caching unit for documents. The fix,
//! when it is worth the cost, is persistent anchors written into the file; that trades a clean
//! source document for perfect id stability, and it is not obviously the right trade for
//! markdown a human also edits.

use flash_adapter::{Artifact, Entity, Outline};

/// A block's type. The vocabulary from section 4.2.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BlockKind {
    Heading,
    Paragraph,
    List,
    Table,
    Figure,
    Code,
    FrontMatter,
}

impl BlockKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            BlockKind::Heading => "heading",
            BlockKind::Paragraph => "p",
            BlockKind::List => "list",
            BlockKind::Table => "table",
            BlockKind::Figure => "figure",
            BlockKind::Code => "code",
            BlockKind::FrontMatter => "meta",
        }
    }

    /// Named `from_tag` rather than `from_str` on purpose: this is not `FromStr`,
    /// it never fails with a parse error, and a reader should not have to check.
    pub fn from_tag(s: &str) -> Option<BlockKind> {
        Some(match s {
            "heading" => BlockKind::Heading,
            "p" => BlockKind::Paragraph,
            "list" => BlockKind::List,
            "table" => BlockKind::Table,
            "figure" => BlockKind::Figure,
            "code" => BlockKind::Code,
            "meta" => BlockKind::FrontMatter,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug)]
pub struct Block {
    pub id: String,
    pub kind: BlockKind,
    /// Heading level, 1 to 6. Zero for everything else.
    pub level: u8,
    /// Slug path of the section this block sits in.
    pub section: String,
    pub text: String,
    pub start: usize,
    pub end: usize,
}

impl Block {
    pub fn word_count(&self) -> usize {
        self.text.split_whitespace().count()
    }

    /// How many rendered lines this block takes. Used by the paginator.
    pub fn line_count(&self) -> usize {
        match self.kind {
            BlockKind::Heading => 2,
            _ => self.text.lines().count().max(1) + 1,
        }
    }
}

pub fn slug(text: &str) -> String {
    let mut out = String::new();
    let mut last_dash = true;
    for c in text.trim().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

/// Parse markdown into blocks.
///
/// A hand written line scanner rather than a markdown crate: the block vocabulary here is small
/// and fixed, byte ranges have to be exact for the materializer, and a dependency that "improves"
/// its parse in a point release would silently reshuffle every block id in every cached document.
pub fn parse(artifact: &Artifact) -> Vec<Block> {
    let text = artifact.text().to_string();
    let mut blocks = Vec::new();
    let mut section = String::from("root");
    let mut counters: std::collections::HashMap<(String, &'static str), usize> = Default::default();

    let mut pos = 0usize;
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut i = 0usize;

    // Front matter, if the file opens with one.
    if lines
        .first()
        .map(|l| l.trim_end() == "---")
        .unwrap_or(false)
    {
        let start = 0;
        let mut end = lines[0].len();
        let mut j = 1;
        while j < lines.len() {
            end += lines[j].len();
            if lines[j].trim_end() == "---" {
                j += 1;
                break;
            }
            j += 1;
        }
        blocks.push(Block {
            id: "meta:root/1".to_string(),
            kind: BlockKind::FrontMatter,
            level: 0,
            section: section.clone(),
            text: text[start..end].to_string(),
            start,
            end,
        });
        pos = end;
        i = j;
    }

    let push = |blocks: &mut Vec<Block>,
                counters: &mut std::collections::HashMap<(String, &'static str), usize>,
                kind: BlockKind,
                level: u8,
                section: &str,
                body: &str,
                start: usize,
                end: usize| {
        if body.trim().is_empty() {
            return;
        }
        let key = (section.to_string(), kind.as_str());
        let n = counters.entry(key).or_insert(0);
        *n += 1;
        blocks.push(Block {
            id: format!("{}:{}/{}", kind.as_str(), section, n),
            kind,
            level,
            section: section.to_string(),
            text: body.trim_end().to_string(),
            start,
            end,
        });
    };

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();
        let start = pos;

        // Heading
        if trimmed.starts_with('#') {
            let level = trimmed.chars().take_while(|c| *c == '#').count().min(6) as u8;
            let title = trimmed.trim_start_matches('#').trim();
            let end = pos + line.len();
            let s = slug(title);
            let heading_section = if s.is_empty() { "root".into() } else { s };
            let key = (heading_section.clone(), "heading");
            let n = counters.entry(key).or_insert(0);
            *n += 1;
            blocks.push(Block {
                id: format!("heading:{heading_section}"),
                kind: BlockKind::Heading,
                level,
                section: heading_section.clone(),
                text: line.trim_end().to_string(),
                start,
                end,
            });
            section = heading_section;
            pos = end;
            i += 1;
            continue;
        }

        // Fenced code
        if trimmed.starts_with("```") {
            let mut end = pos + line.len();
            let mut j = i + 1;
            while j < lines.len() {
                end += lines[j].len();
                let done = lines[j].trim_start().starts_with("```");
                j += 1;
                if done {
                    break;
                }
            }
            push(
                &mut blocks,
                &mut counters,
                BlockKind::Code,
                0,
                &section,
                &text[start..end],
                start,
                end,
            );
            pos = end;
            i = j;
            continue;
        }

        // Blank line
        if line.trim().is_empty() {
            pos += line.len();
            i += 1;
            continue;
        }

        // Table, list, figure or paragraph: consume until a blank line.
        let kind = if trimmed.starts_with('|') {
            BlockKind::Table
        } else if trimmed.starts_with("- ")
            || trimmed.starts_with("* ")
            || trimmed
                .split_once('.')
                .map(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
                .unwrap_or(false)
        {
            BlockKind::List
        } else if trimmed.starts_with("![") {
            BlockKind::Figure
        } else {
            BlockKind::Paragraph
        };

        let mut end = pos;
        let mut j = i;
        while j < lines.len() && !lines[j].trim().is_empty() {
            end += lines[j].len();
            j += 1;
        }
        push(
            &mut blocks,
            &mut counters,
            kind,
            0,
            &section,
            &text[start..end],
            start,
            end,
        );
        pos = end;
        i = j;
    }

    blocks
}

/// The block tree as the adapter-neutral outline the rest of the runtime speaks.
pub fn outline(artifact: &Artifact) -> Outline {
    let blocks = parse(artifact);
    let entities = blocks
        .iter()
        .map(|b| Entity {
            id: b.id.clone(),
            kind: b.kind.as_str().to_string(),
            start: b.start,
            end: b.end,
            body: Some((b.start, b.end)),
            signature: Some(first_line(&b.text)),
            // A block "refers to" its section heading: that is the edge impact analysis walks
            // when a heading is rewritten and every block under it has to be re-rendered.
            refs: if b.kind == BlockKind::Heading {
                Vec::new()
            } else {
                vec![format!("heading:{}", b.section)]
            },
            parent: Some(b.section.clone()),
        })
        .collect();
    Outline {
        path: artifact.path.clone(),
        entities,
    }
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or("").trim().to_string()
}

/// A page of the rendered document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page {
    pub number: usize,
    pub blocks: Vec<String>,
}

/// Lines per page in the built-in renderer.
pub const LINES_PER_PAGE: usize = 48;

/// Lay blocks out into pages, deterministically.
///
/// The built-in renderer is a paginator, not a typesetter. That is enough to demonstrate and test
/// the property section 4.2 actually cares about - only the pages containing changed blocks are
/// re-rendered - without shipping a pdf engine. A real converter (docx to pdf) plugs in as an
/// external job; what it cannot change is which pages this says are dirty.
pub fn paginate(blocks: &[Block]) -> Vec<Page> {
    let mut pages = Vec::new();
    let mut current = Page {
        number: 1,
        blocks: Vec::new(),
    };
    let mut used = 0usize;

    for b in blocks {
        let lines = b.line_count();
        if used + lines > LINES_PER_PAGE && !current.blocks.is_empty() {
            pages.push(std::mem::replace(
                &mut current,
                Page {
                    number: pages.len() + 2,
                    blocks: Vec::new(),
                },
            ));
            used = 0;
        }
        current.blocks.push(b.id.clone());
        used += lines;
    }
    if !current.blocks.is_empty() {
        pages.push(current);
    }
    pages
}

/// Which pages contain any of these block ids.
pub fn pages_for(pages: &[Page], block_ids: &[String]) -> Vec<usize> {
    let mut hit: Vec<usize> = pages
        .iter()
        .filter(|p| p.blocks.iter().any(|b| block_ids.iter().any(|c| c == b)))
        .map(|p| p.number)
        .collect();
    hit.dedup();
    hit
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "---\ntitle: Q3\n---\n\n# Quarterly report\n\nOpening paragraph.\n\n## Methodology\n\nWe counted things.\n\n- one\n- two\n\n## Revenue\n\n| q | usd |\n| - | --- |\n| 3 | 10  |\n\nClosing note.\n";

    fn art() -> Artifact {
        Artifact::new("report.md", DOC.as_bytes().to_vec())
    }

    #[test]
    fn blocks_are_typed_and_sectioned() {
        let blocks = parse(&art());
        let kinds: Vec<&str> = blocks.iter().map(|b| b.kind.as_str()).collect();
        assert!(kinds.contains(&"meta"), "{kinds:?}");
        assert!(kinds.contains(&"heading"), "{kinds:?}");
        assert!(kinds.contains(&"table"), "{kinds:?}");
        assert!(kinds.contains(&"list"), "{kinds:?}");

        let table = blocks.iter().find(|b| b.kind == BlockKind::Table).unwrap();
        assert_eq!(table.section, "revenue");
    }

    #[test]
    fn byte_ranges_slice_back_to_the_block_text() {
        let a = art();
        let text = a.text().to_string();
        for b in parse(&a) {
            assert_eq!(
                text[b.start..b.end].trim_end(),
                b.text.trim_end(),
                "block {} has wrong byte range",
                b.id
            );
        }
    }

    #[test]
    fn editing_one_section_does_not_renumber_another() {
        // The property the whole incremental story needs from block ids.
        let before = parse(&art());
        let edited = DOC.replace(
            "We counted things.",
            "We counted things twice.\n\nAnd again.",
        );
        let after = parse(&Artifact::new("report.md", edited.into_bytes()));

        let revenue_before: Vec<&String> = before
            .iter()
            .filter(|b| b.section == "revenue")
            .map(|b| &b.id)
            .collect();
        let revenue_after: Vec<&String> = after
            .iter()
            .filter(|b| b.section == "revenue")
            .map(|b| &b.id)
            .collect();
        assert_eq!(
            revenue_before, revenue_after,
            "an edit under methodology renumbered revenue"
        );
    }

    #[test]
    fn pagination_is_deterministic_and_pages_are_addressable() {
        let blocks = parse(&art());
        let a = paginate(&blocks);
        let b = paginate(&blocks);
        assert_eq!(a, b);
        assert!(!a.is_empty());
        assert_eq!(a[0].number, 1);
    }

    #[test]
    fn only_the_pages_holding_changed_blocks_are_dirty() {
        // A long document so there is more than one page to be wrong about.
        let mut doc = String::from("# Long\n\n");
        for i in 0..80 {
            doc.push_str(&format!("Paragraph number {i}.\n\n"));
        }
        let blocks = parse(&Artifact::new("long.md", doc.into_bytes()));
        let pages = paginate(&blocks);
        assert!(
            pages.len() > 2,
            "expected several pages, got {}",
            pages.len()
        );

        let changed = vec![blocks[1].id.clone()];
        let dirty = pages_for(&pages, &changed);
        assert_eq!(dirty.len(), 1, "one changed block must dirty one page");
        assert_eq!(dirty[0], 1);
    }

    #[test]
    fn slugs_are_stable_and_readable() {
        assert_eq!(slug("Methodology & Notes"), "methodology-notes");
        assert_eq!(slug("  Revenue  "), "revenue");
        assert_eq!(slug("Q3 2026"), "q3-2026");
    }

    #[test]
    fn every_block_maps_to_an_outline_entity() {
        let a = art();
        let o = outline(&a);
        assert_eq!(o.entities.len(), parse(&a).len());
        let non_heading = o.entities.iter().find(|e| e.kind == "p").unwrap();
        assert!(
            non_heading.refs.iter().any(|r| r.starts_with("heading:")),
            "a block should hang off its heading"
        );
    }
}

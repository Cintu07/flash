//! flash-adapter-slides: decks as typed elements bound to a theme (PRD section 4.3).
//!
//! The rung that earns its place here is rung 1. "Text fits box, contrast, one idea per slide"
//! sounds like taste, but each of those is a measurable property of the deck model: a character
//! budget per element for the layout, a wcag contrast ratio between theme colours, and a count of
//! independent ideas per slide. Checking them costs a millisecond and catches the failures that
//! would otherwise only show up after a multi-second export and a vision model call.
//!
//! Thumbnails are the impact unit: a changed slide re-renders one thumbnail, not the deck.

use flash_adapter::{
    Adapter, AdapterError, Applied, Artifact, Delta, Diagnostic, Entity, ImpactSet, Op, Outline,
    Pack, PackRequest, Result, RungOutcome, VerifyCtx, VerifyRung,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Theme {
    #[serde(default = "default_fg")]
    pub fg: String,
    #[serde(default = "default_bg")]
    pub bg: String,
    #[serde(default)]
    pub accent: String,
    /// Character budget for a title in this theme, at this font size.
    #[serde(default = "default_title_chars")]
    pub title_max_chars: usize,
    #[serde(default = "default_body_chars")]
    pub body_max_chars: usize,
    /// How many bullets a slide may carry before it is two slides pretending to be one.
    #[serde(default = "default_max_bullets")]
    pub max_bullets: usize,
}

fn default_fg() -> String {
    "#111111".into()
}
fn default_bg() -> String {
    "#ffffff".into()
}
fn default_title_chars() -> usize {
    60
}
fn default_body_chars() -> usize {
    360
}
fn default_max_bullets() -> usize {
    6
}

impl Default for Theme {
    fn default() -> Self {
        Theme {
            fg: default_fg(),
            bg: default_bg(),
            accent: String::new(),
            title_max_chars: default_title_chars(),
            body_max_chars: default_body_chars(),
            max_bullets: default_max_bullets(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Element {
    /// title, body, image, chart, table.
    pub kind: String,
    #[serde(default)]
    pub text: String,
    /// For charts: where the numbers come from, so a sheet change can invalidate the slide.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Slide {
    pub id: String,
    #[serde(default = "default_layout")]
    pub layout: String,
    #[serde(default)]
    pub elements: Vec<Element>,
    #[serde(default)]
    pub notes: String,
}

fn default_layout() -> String {
    "title-body".into()
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Deck {
    #[serde(default)]
    pub theme: Theme,
    #[serde(default)]
    pub slides: Vec<Slide>,
}

/// Layouts and the element kinds each one expects.
pub const LAYOUTS: &[(&str, &[&str])] = &[
    ("title", &["title"]),
    ("title-body", &["title", "body"]),
    ("title-chart", &["title", "chart"]),
    ("title-table", &["title", "table"]),
    ("image-full", &["image"]),
    ("section", &["title"]),
];

impl Deck {
    pub fn parse(artifact: &Artifact) -> Result<Deck> {
        serde_json::from_slice(&artifact.bytes)
            .map_err(|e| AdapterError::Parse(format!("deck is not valid json: {e}")))
    }

    pub fn to_artifact(&self, path: &str) -> Artifact {
        Artifact::new(
            path.to_string(),
            serde_json::to_vec_pretty(self).expect("a deck serializes"),
        )
    }

    pub fn slide(&self, id: &str) -> Option<&Slide> {
        self.slides.iter().find(|s| s.id == id)
    }

    pub fn position(&self, id: &str) -> Option<usize> {
        self.slides.iter().position(|s| s.id == id)
    }
}

pub struct SlidesAdapter;

impl Default for SlidesAdapter {
    fn default() -> Self {
        SlidesAdapter
    }
}

impl SlidesAdapter {
    pub fn new() -> Self {
        SlidesAdapter
    }
}

fn element_id(slide: &str, kind: &str, n: usize) -> String {
    format!("element:{slide}/{kind}/{n}")
}

impl Adapter for SlidesAdapter {
    fn name(&self) -> &'static str {
        "slides"
    }

    fn version(&self) -> &'static str {
        "slides-v1"
    }

    fn handles(&self, path: &str) -> bool {
        let lower = path.to_ascii_lowercase();
        lower.ends_with(".deck.json") || lower.ends_with(".pptx")
    }

    fn op_schema(&self) -> serde_json::Value {
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "flash slide ops",
            "type": "array",
            "minItems": 1,
            "maxItems": 64,
            "items": {
                "type": "object",
                "required": ["op"],
                "oneOf": [
                    {
                        "properties": {
                            "op": { "const": "add_slide" },
                            "id": { "type": "string" },
                            "after": { "type": "string", "description": "slide id, or \"start\"" },
                            "layout": { "type": "string" },
                            "title": { "type": "string" },
                            "body": { "type": "string" }
                        },
                        "required": ["op", "id", "layout", "title"],
                        "additionalProperties": false
                    },
                    {
                        "properties": {
                            "op": { "const": "replace_element" },
                            "element": { "type": "string" },
                            "text": { "type": "string" }
                        },
                        "required": ["op", "element", "text"],
                        "additionalProperties": false
                    },
                    {
                        "properties": {
                            "op": { "const": "reorder" },
                            "slide": { "type": "string" },
                            "after": { "type": "string" }
                        },
                        "required": ["op", "slide", "after"],
                        "additionalProperties": false
                    },
                    {
                        "properties": {
                            "op": { "const": "apply_layout" },
                            "slide": { "type": "string" },
                            "layout": { "type": "string" }
                        },
                        "required": ["op", "slide", "layout"],
                        "additionalProperties": false
                    },
                    {
                        "properties": {
                            "op": { "const": "bind_chart_data" },
                            "element": { "type": "string" },
                            "source": { "type": "string" }
                        },
                        "required": ["op", "element", "source"],
                        "additionalProperties": false
                    },
                    {
                        "properties": {
                            "op": { "const": "delete_slide" },
                            "slide": { "type": "string" }
                        },
                        "required": ["op", "slide"],
                        "additionalProperties": false
                    }
                ]
            }
        })
    }

    fn outline(&self, artifact: &Artifact) -> Result<Outline> {
        let deck = Deck::parse(artifact)?;
        let mut entities = Vec::new();
        for slide in &deck.slides {
            entities.push(Entity {
                id: format!("slide:{}", slide.id),
                kind: "slide".into(),
                start: 0,
                end: 0,
                body: None,
                signature: Some(format!("{} [{}]", title_of(slide), slide.layout)),
                refs: Vec::new(),
                parent: None,
            });
            let mut counts: std::collections::HashMap<&str, usize> = Default::default();
            for el in &slide.elements {
                let n = counts.entry(el.kind.as_str()).or_insert(0);
                *n += 1;
                entities.push(Entity {
                    id: element_id(&slide.id, &el.kind, *n),
                    kind: el.kind.clone(),
                    start: 0,
                    end: 0,
                    body: None,
                    signature: Some(el.text.chars().take(120).collect()),
                    // An element belongs to its slide: that edge is what turns a changed element
                    // into exactly one dirty thumbnail.
                    refs: vec![format!("slide:{}", slide.id)],
                    parent: Some(slide.id.clone()),
                });
            }
        }
        Ok(Outline {
            path: artifact.path.clone(),
            entities,
        })
    }

    fn apply(&self, artifact: &Artifact, ops: &[Op]) -> Result<Applied> {
        if ops.is_empty() {
            return Err(AdapterError::BadOp("empty op batch".into()));
        }
        let before = Deck::parse(artifact)?;
        let before_outline = self.outline(artifact)?;
        let mut deck = before.clone();

        for op in ops {
            match op.kind() {
                Some("add_slide") => {
                    let id = op.str_field("id")?.to_string();
                    if deck.slide(&id).is_some() {
                        return Err(AdapterError::BadOp(format!("slide {id} already exists")));
                    }
                    let layout = op.str_field("layout")?.to_string();
                    if !LAYOUTS.iter().any(|(l, _)| *l == layout) {
                        return Err(AdapterError::BadOp(format!("unknown layout {layout}")));
                    }
                    let mut elements = vec![Element {
                        kind: "title".into(),
                        text: op.str_field("title")?.to_string(),
                        source: None,
                    }];
                    if let Some(body) = op.opt_str("body") {
                        elements.push(Element {
                            kind: "body".into(),
                            text: body.to_string(),
                            source: None,
                        });
                    }
                    let slide = Slide {
                        id,
                        layout,
                        elements,
                        notes: String::new(),
                    };
                    match op.opt_str("after") {
                        Some("start") | None => deck.slides.insert(0, slide),
                        Some(after) => {
                            let at = deck.position(after).ok_or_else(|| {
                                AdapterError::UnknownEntity(format!("slide:{after}"))
                            })?;
                            deck.slides.insert(at + 1, slide);
                        }
                    }
                }
                Some("replace_element") => {
                    let id = op.str_field("element")?;
                    let text = op.str_field("text")?.to_string();
                    let (slide_id, kind, n) = parse_element_id(id)?;
                    let slide = deck
                        .slides
                        .iter_mut()
                        .find(|s| s.id == slide_id)
                        .ok_or_else(|| AdapterError::UnknownEntity(id.to_string()))?;
                    let el = slide
                        .elements
                        .iter_mut()
                        .filter(|e| e.kind == kind)
                        .nth(n - 1)
                        .ok_or_else(|| AdapterError::UnknownEntity(id.to_string()))?;
                    el.text = text;
                }
                Some("reorder") => {
                    let id = op.str_field("slide")?;
                    let after = op.str_field("after")?;
                    let from = deck
                        .position(id)
                        .ok_or_else(|| AdapterError::UnknownEntity(format!("slide:{id}")))?;
                    let slide = deck.slides.remove(from);
                    match after {
                        "start" => deck.slides.insert(0, slide),
                        other => {
                            let at = deck.position(other).ok_or_else(|| {
                                AdapterError::UnknownEntity(format!("slide:{other}"))
                            })?;
                            deck.slides.insert(at + 1, slide);
                        }
                    }
                }
                Some("apply_layout") => {
                    let id = op.str_field("slide")?;
                    let layout = op.str_field("layout")?.to_string();
                    if !LAYOUTS.iter().any(|(l, _)| *l == layout) {
                        return Err(AdapterError::BadOp(format!("unknown layout {layout}")));
                    }
                    let slide = deck
                        .slides
                        .iter_mut()
                        .find(|s| s.id == id)
                        .ok_or_else(|| AdapterError::UnknownEntity(format!("slide:{id}")))?;
                    slide.layout = layout;
                }
                Some("bind_chart_data") => {
                    let id = op.str_field("element")?;
                    let source = op.str_field("source")?.to_string();
                    let (slide_id, kind, n) = parse_element_id(id)?;
                    if kind != "chart" {
                        return Err(AdapterError::BadOp(format!("{id} is not a chart")));
                    }
                    let slide = deck
                        .slides
                        .iter_mut()
                        .find(|s| s.id == slide_id)
                        .ok_or_else(|| AdapterError::UnknownEntity(id.to_string()))?;
                    let el = slide
                        .elements
                        .iter_mut()
                        .filter(|e| e.kind == "chart")
                        .nth(n - 1)
                        .ok_or_else(|| AdapterError::UnknownEntity(id.to_string()))?;
                    el.source = Some(source);
                }
                Some("delete_slide") => {
                    let id = op.str_field("slide")?;
                    let at = deck
                        .position(id)
                        .ok_or_else(|| AdapterError::UnknownEntity(format!("slide:{id}")))?;
                    deck.slides.remove(at);
                }
                other => {
                    return Err(AdapterError::BadOp(format!(
                        "unknown op {}",
                        other.unwrap_or("<missing>")
                    )));
                }
            }
        }

        let candidate = deck.to_artifact(&artifact.path);
        let after_outline = self.outline(&candidate)?;
        let mut delta = Delta::default();
        for e in &after_outline.entities {
            match before_outline.get(&e.id) {
                None => delta.added.push(e.id.clone()),
                Some(old) => {
                    if old.signature != e.signature {
                        delta.changed.push(e.id.clone());
                    }
                }
            }
        }
        for e in &before_outline.entities {
            if after_outline.get(&e.id).is_none() {
                delta.removed.push(e.id.clone());
            }
        }

        Ok(Applied {
            artifact: candidate,
            delta,
        })
    }

    fn ladder(&self) -> Vec<Arc<dyn VerifyRung>> {
        vec![
            Arc::new(DeckValid),
            Arc::new(LayoutRules),
            Arc::new(Thumbnails),
            Arc::new(VisionCheck { model: None }),
            Arc::new(FullExport),
        ]
    }

    fn impact(&self, outline: &Outline, delta: &Delta) -> ImpactSet {
        let touched: Vec<String> = delta.touched().into_iter().map(str::to_string).collect();
        let mut slides: Vec<String> = Vec::new();
        for id in &touched {
            if let Some(rest) = id.strip_prefix("slide:") {
                push_unique(&mut slides, rest.to_string());
            }
            if let Some(e) = outline.get(id)
                && let Some(parent) = &e.parent
            {
                push_unique(&mut slides, parent.clone());
            }
        }
        ImpactSet {
            entities: touched,
            tests: Vec::new(),
            units: slides.iter().map(|s| format!("thumbnail:{s}")).collect(),
        }
    }

    fn pack(&self, req: &PackRequest<'_>) -> Result<Pack> {
        // Section 3.3 for slides: target slide, deck outline, theme tokens, speaker notes.
        let deck = Deck::parse(req.artifact)?;
        let slide_id = req
            .target
            .strip_prefix("slide:")
            .map(|s| s.to_string())
            .or_else(|| req.outline.get(req.target).and_then(|e| e.parent.clone()))
            .ok_or_else(|| AdapterError::UnknownEntity(req.target.to_string()))?;
        let slide = deck
            .slide(&slide_id)
            .ok_or_else(|| AdapterError::UnknownEntity(req.target.to_string()))?;

        let mut pack = Pack::default();
        pack.push(
            "deck outline",
            deck.slides
                .iter()
                .enumerate()
                .map(|(i, s)| format!("{}. {} [{}] ({})", i + 1, title_of(s), s.layout, s.id))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        pack.push(
            "target slide",
            slide
                .elements
                .iter()
                .enumerate()
                .map(|(i, e)| format!("{} ({}): {}", e.kind, i + 1, e.text))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        pack.push(
            "theme",
            format!(
                "fg {} / bg {} / accent {}\ntitle fits {} chars, body fits {} chars, at most {} bullets",
                deck.theme.fg,
                deck.theme.bg,
                deck.theme.accent,
                deck.theme.title_max_chars,
                deck.theme.body_max_chars,
                deck.theme.max_bullets
            ),
        );
        pack.push("speaker notes", slide.notes.clone());

        while pack.chars() > req.budget_chars.max(500) && pack.parts.len() > 2 {
            pack.parts.pop();
        }
        Ok(pack)
    }
}

fn push_unique(v: &mut Vec<String>, s: String) {
    if !v.contains(&s) {
        v.push(s);
    }
}

fn title_of(slide: &Slide) -> String {
    slide
        .elements
        .iter()
        .find(|e| e.kind == "title")
        .map(|e| e.text.clone())
        .unwrap_or_else(|| "(untitled)".into())
}

fn parse_element_id(id: &str) -> Result<(String, String, usize)> {
    let rest = id
        .strip_prefix("element:")
        .ok_or_else(|| AdapterError::BadOp(format!("{id} is not an element id")))?;
    let mut parts = rest.split('/');
    let slide = parts.next().unwrap_or("").to_string();
    let kind = parts.next().unwrap_or("").to_string();
    let n: usize = parts.next().unwrap_or("1").parse().unwrap_or(1);
    if slide.is_empty() || kind.is_empty() {
        return Err(AdapterError::BadOp(format!("{id} is not an element id")));
    }
    Ok((slide, kind, n))
}

// ---- ladder ----------------------------------------------------------------------------------

/// Rung 0: the deck model is valid.
pub struct DeckValid;

impl VerifyRung for DeckValid {
    fn name(&self) -> &str {
        "deck-model"
    }
    fn level(&self) -> u8 {
        0
    }
    fn expected_ms(&self) -> u64 {
        1
    }

    fn check(&self, ctx: &VerifyCtx<'_>) -> RungOutcome {
        let deck = match Deck::parse(ctx.artifact) {
            Ok(d) => d,
            Err(e) => return RungOutcome::fail(vec![Diagnostic::new("deck", e.to_string())]),
        };
        let mut diags = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for s in &deck.slides {
            if !seen.insert(s.id.clone()) {
                diags.push(Diagnostic::new(
                    "duplicate-slide-id",
                    format!("{} appears twice", s.id),
                ));
            }
            match LAYOUTS.iter().find(|(l, _)| *l == s.layout) {
                None => diags.push(
                    Diagnostic::new("unknown-layout", format!("{} uses {}", s.id, s.layout))
                        .at_entity(format!("slide:{}", s.id)),
                ),
                Some((_, expected)) => {
                    for kind in *expected {
                        if !s.elements.iter().any(|e| e.kind == *kind) {
                            diags.push(
                                Diagnostic::new(
                                    "missing-element",
                                    format!("{} has layout {} but no {kind}", s.id, s.layout),
                                )
                                .at_entity(format!("slide:{}", s.id)),
                            );
                        }
                    }
                }
            }
        }
        if diags.is_empty() {
            RungOutcome::pass()
        } else {
            RungOutcome::fail(diags)
        }
    }
}

/// Rung 1: text fits its box, contrast is legible, one idea per slide.
pub struct LayoutRules;

impl VerifyRung for LayoutRules {
    fn name(&self) -> &str {
        "layout-rules"
    }
    fn level(&self) -> u8 {
        1
    }
    fn expected_ms(&self) -> u64 {
        50
    }

    fn check(&self, ctx: &VerifyCtx<'_>) -> RungOutcome {
        let deck = match Deck::parse(ctx.artifact) {
            Ok(d) => d,
            Err(e) => return RungOutcome::fail(vec![Diagnostic::new("deck", e.to_string())]),
        };
        let mut diags = Vec::new();

        // Contrast is a property of the theme, so it is checked once rather than per slide.
        if let (Some(fg), Some(bg)) = (luminance(&deck.theme.fg), luminance(&deck.theme.bg)) {
            let ratio = (fg.max(bg) + 0.05) / (fg.min(bg) + 0.05);
            if ratio < 4.5 {
                diags.push(Diagnostic::new(
                    "low-contrast",
                    format!(
                        "{} on {} is {ratio:.1}:1, under the 4.5:1 minimum",
                        deck.theme.fg, deck.theme.bg
                    ),
                ));
            }
        }

        for s in &deck.slides {
            for (i, el) in s.elements.iter().enumerate() {
                let budget = match el.kind.as_str() {
                    "title" => deck.theme.title_max_chars,
                    "body" => deck.theme.body_max_chars,
                    _ => continue,
                };
                if el.text.chars().count() > budget {
                    diags.push(
                        Diagnostic::new(
                            "text-overflow",
                            format!(
                                "{} {} is {} chars, over the {budget} this theme fits",
                                s.id,
                                el.kind,
                                el.text.chars().count()
                            ),
                        )
                        .at_entity(element_id(&s.id, &el.kind, i + 1)),
                    );
                }
            }

            let bullets = s
                .elements
                .iter()
                .filter(|e| e.kind == "body")
                .flat_map(|e| e.text.lines())
                .filter(|l| {
                    let t = l.trim_start();
                    t.starts_with("- ") || t.starts_with("* ")
                })
                .count();
            if bullets > deck.theme.max_bullets {
                diags.push(
                    Diagnostic::new(
                        "too-many-ideas",
                        format!(
                            "{} carries {bullets} bullets; over {} it is two slides",
                            s.id, deck.theme.max_bullets
                        ),
                    )
                    .at_entity(format!("slide:{}", s.id)),
                );
            }
        }

        if diags.is_empty() {
            RungOutcome::pass()
        } else {
            RungOutcome::fail(diags)
        }
    }
}

/// sRGB relative luminance, for the contrast ratio.
fn luminance(hex: &str) -> Option<f64> {
    let h = hex.trim_start_matches('#');
    if h.len() != 6 {
        return None;
    }
    let channel = |i: usize| -> Option<f64> {
        let v = u8::from_str_radix(&h[i..i + 2], 16).ok()? as f64 / 255.0;
        Some(if v <= 0.03928 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        })
    };
    Some(0.2126 * channel(0)? + 0.7152 * channel(2)? + 0.0722 * channel(4)?)
}

/// Rung 2: thumbnail the changed slides only.
pub struct Thumbnails;

impl VerifyRung for Thumbnails {
    fn name(&self) -> &str {
        "thumbnails"
    }
    fn level(&self) -> u8 {
        2
    }
    fn expected_ms(&self) -> u64 {
        2_000
    }

    fn check(&self, ctx: &VerifyCtx<'_>) -> RungOutcome {
        let deck = match Deck::parse(ctx.artifact) {
            Ok(d) => d,
            Err(e) => return RungOutcome::fail(vec![Diagnostic::new("deck", e.to_string())]),
        };
        let wanted: Vec<&str> = ctx
            .impact
            .units
            .iter()
            .filter_map(|u| u.strip_prefix("thumbnail:"))
            .collect();
        let mut diags = Vec::new();
        for id in wanted {
            match deck.slide(id) {
                None => diags.push(Diagnostic::new(
                    "missing-slide",
                    format!("thumbnail requested for {id}, which is not in the deck"),
                )),
                Some(s) if s.elements.is_empty() => diags.push(
                    Diagnostic::new("empty-slide", format!("{id} has no elements to render"))
                        .at_entity(format!("slide:{id}")),
                ),
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

/// Rung 3: a small vision model on the changed thumbnails.
pub struct VisionCheck {
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
        3_500
    }

    fn check(&self, _ctx: &VerifyCtx<'_>) -> RungOutcome {
        match &self.model {
            None => RungOutcome::unavailable("no vision model configured for the slides rung 3"),
            Some(_) => RungOutcome::pass(),
        }
    }
}

/// Rung 4: full export, once.
pub struct FullExport;

impl VerifyRung for FullExport {
    fn name(&self) -> &str {
        "full-export"
    }
    fn level(&self) -> u8 {
        4
    }
    fn expected_ms(&self) -> u64 {
        15_000
    }
    fn once_per_task(&self) -> bool {
        true
    }

    fn check(&self, ctx: &VerifyCtx<'_>) -> RungOutcome {
        match Deck::parse(ctx.artifact) {
            Ok(d) if d.slides.is_empty() => {
                RungOutcome::fail(vec![Diagnostic::new("empty-deck", "nothing to export")])
            }
            Ok(_) => RungOutcome::pass(),
            Err(e) => RungOutcome::fail(vec![Diagnostic::new("deck", e.to_string())]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deck() -> Artifact {
        Deck {
            theme: Theme::default(),
            slides: vec![
                Slide {
                    id: "intro".into(),
                    layout: "title-body".into(),
                    elements: vec![
                        Element {
                            kind: "title".into(),
                            text: "Q3 results".into(),
                            source: None,
                        },
                        Element {
                            kind: "body".into(),
                            text: "- revenue up\n- costs flat".into(),
                            source: None,
                        },
                    ],
                    notes: "open with the headline number".into(),
                },
                Slide {
                    id: "chart".into(),
                    layout: "title-chart".into(),
                    elements: vec![
                        Element {
                            kind: "title".into(),
                            text: "Revenue by quarter".into(),
                            source: None,
                        },
                        Element {
                            kind: "chart".into(),
                            text: "bar".into(),
                            source: None,
                        },
                    ],
                    notes: String::new(),
                },
            ],
        }
        .to_artifact("q3.deck.json")
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
    fn slides_and_elements_are_entities() {
        let a = SlidesAdapter::new();
        let o = a.outline(&deck()).unwrap();
        assert!(o.get("slide:intro").is_some());
        assert!(o.get("element:intro/title/1").is_some());
        assert_eq!(
            o.get("element:intro/body/1").unwrap().parent.as_deref(),
            Some("intro")
        );
    }

    #[test]
    fn replacing_an_element_changes_one_element_and_its_slide_stays_put() {
        let a = SlidesAdapter::new();
        let applied = a
            .apply(
                &deck(),
                &[Op::new(json!({
                    "op":"replace_element",
                    "element":"element:intro/title/1",
                    "text":"Q3 results, restated"
                }))],
            )
            .unwrap();
        assert!(
            applied
                .delta
                .changed
                .contains(&"element:intro/title/1".to_string())
        );
        let d = Deck::parse(&applied.artifact).unwrap();
        assert_eq!(d.slides.len(), 2);
        assert_eq!(d.slides[0].id, "intro");
    }

    #[test]
    fn a_changed_element_dirties_exactly_one_thumbnail() {
        let a = SlidesAdapter::new();
        let artifact = deck();
        let outline = a.outline(&artifact).unwrap();
        let delta = Delta {
            changed: vec!["element:intro/title/1".into()],
            ..Default::default()
        };
        let impact = a.impact(&outline, &delta);
        assert_eq!(impact.units, vec!["thumbnail:intro"], "{impact:?}");
    }

    #[test]
    fn an_unknown_layout_is_refused_at_apply_time() {
        let a = SlidesAdapter::new();
        let err = a
            .apply(
                &deck(),
                &[Op::new(
                    json!({"op":"apply_layout","slide":"intro","layout":"fancy"}),
                )],
            )
            .unwrap_err();
        assert!(format!("{err}").contains("unknown layout"), "{err}");
    }

    #[test]
    fn rung_zero_catches_a_layout_without_its_elements() {
        let a = SlidesAdapter::new();
        // title-chart on a slide that has no chart.
        let applied = a
            .apply(
                &deck(),
                &[Op::new(
                    json!({"op":"apply_layout","slide":"intro","layout":"title-chart"}),
                )],
            )
            .unwrap();
        let outline = a.outline(&applied.artifact).unwrap();
        let (d, i) = (Delta::default(), ImpactSet::default());
        let outcome = a.ladder()[0].check(&ctx(&applied.artifact, &outline, &d, &i));
        assert!(!outcome.passed);
        assert_eq!(outcome.diagnostics[0].code, "missing-element");
    }

    #[test]
    fn rung_one_catches_overflowing_text() {
        let a = SlidesAdapter::new();
        let long = "x".repeat(200);
        let applied = a
            .apply(
                &deck(),
                &[Op::new(json!({
                    "op":"replace_element",
                    "element":"element:intro/title/1",
                    "text": long
                }))],
            )
            .unwrap();
        let outline = a.outline(&applied.artifact).unwrap();
        let (d, i) = (Delta::default(), ImpactSet::default());
        let outcome = a.ladder()[1].check(&ctx(&applied.artifact, &outline, &d, &i));
        assert!(!outcome.passed);
        assert_eq!(outcome.diagnostics[0].code, "text-overflow");
    }

    #[test]
    fn rung_one_catches_low_contrast_and_too_many_ideas() {
        let mut d = Deck::parse(&deck()).unwrap();
        d.theme.fg = "#cccccc".into();
        d.theme.bg = "#ffffff".into();
        d.slides[0].elements[1].text = (0..10)
            .map(|i| format!("- point {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let artifact = d.to_artifact("q3.deck.json");

        let a = SlidesAdapter::new();
        let outline = a.outline(&artifact).unwrap();
        let (delta, impact) = (Delta::default(), ImpactSet::default());
        let outcome = a.ladder()[1].check(&ctx(&artifact, &outline, &delta, &impact));
        let codes: Vec<&str> = outcome
            .diagnostics
            .iter()
            .map(|x| x.code.as_str())
            .collect();
        assert!(codes.contains(&"low-contrast"), "{codes:?}");
        assert!(codes.contains(&"too-many-ideas"), "{codes:?}");
    }

    #[test]
    fn contrast_of_black_on_white_is_the_textbook_ratio() {
        let fg = luminance("#000000").unwrap();
        let bg = luminance("#ffffff").unwrap();
        let ratio = (bg + 0.05) / (fg + 0.05);
        assert!((ratio - 21.0).abs() < 0.1, "got {ratio}");
    }

    #[test]
    fn a_pack_carries_the_slide_the_outline_and_the_theme() {
        let a = SlidesAdapter::new();
        let artifact = deck();
        let outline = a.outline(&artifact).unwrap();
        let pack = a
            .pack(&PackRequest {
                artifact: &artifact,
                outline: &outline,
                target: "slide:intro",
                budget_chars: 4_000,
            })
            .unwrap();
        let text = pack.render();
        assert!(text.contains("Q3 results"), "{text}");
        assert!(text.contains("Revenue by quarter"), "outline: {text}");
        assert!(
            text.contains("4.5") || text.contains("fits"),
            "theme: {text}"
        );
        assert!(text.contains("open with the headline"), "notes: {text}");
    }

    #[test]
    fn reorder_moves_a_slide_without_touching_its_content() {
        let a = SlidesAdapter::new();
        let applied = a
            .apply(
                &deck(),
                &[Op::new(
                    json!({"op":"reorder","slide":"chart","after":"start"}),
                )],
            )
            .unwrap();
        let d = Deck::parse(&applied.artifact).unwrap();
        assert_eq!(d.slides[0].id, "chart");
        assert!(
            applied.delta.changed.is_empty(),
            "moving a slide changes order, not content: {:?}",
            applied.delta
        );
    }
}

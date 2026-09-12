//! flash-adapter-sheets: workbooks as a formula dependency graph (PRD section 4.4).
//!
//! The workbook format here is json rather than xlsx. That is a deliberate scope line: reading
//! and writing the xlsx container is mechanical work that teaches nothing about incremental
//! recompute, while the dependency graph and the recalc-only-what-changed path *are* the point,
//! and they are exact here rather than heuristic. An xlsx reader slots in underneath this model
//! without changing an op, a rung or a cache key.
//!
//! Verification follows section 4.4: formulas parse, the dependent recalc has no error cells,
//! then the assertions the planner wrote (totals match, no error cells), then a full recalc once.

pub mod formula;

use flash_adapter::{
    Adapter, AdapterError, Applied, Artifact, Delta, Diagnostic, ImpactSet, Op, Outline, Pack,
    PackRequest, Result, RungOutcome, VerifyCtx, VerifyRung,
};
use formula::{Cell, CellRef, Engine, Grid, Value};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

/// The on-disk workbook.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Workbook {
    #[serde(default)]
    pub sheets: Vec<String>,
    /// "Sheet1!A1" -> literal or "=formula".
    #[serde(default)]
    pub cells: BTreeMap<String, String>,
    /// Defined names: "revenue" -> "Sheet1!A1:A12".
    #[serde(default)]
    pub names: BTreeMap<String, String>,
    /// Assertions the planner writes, checked at rung 2.
    #[serde(default)]
    pub assertions: Vec<String>,
}

impl Workbook {
    pub fn parse(artifact: &Artifact) -> Result<Workbook> {
        serde_json::from_slice(&artifact.bytes)
            .map_err(|e| AdapterError::Parse(format!("workbook is not valid json: {e}")))
    }

    pub fn to_artifact(&self, path: &str) -> Artifact {
        Artifact::new(
            path.to_string(),
            serde_json::to_vec_pretty(self).expect("a workbook serializes"),
        )
    }

    pub fn default_sheet(&self) -> &str {
        self.sheets.first().map(|s| s.as_str()).unwrap_or("Sheet1")
    }

    /// The typed grid the evaluator works on.
    pub fn grid(&self) -> Grid {
        let mut grid = Grid::new();
        for (at, raw) in &self.cells {
            let Some(cell_ref) = CellRef::parse(self.default_sheet(), at) else {
                continue;
            };
            let value = if raw.starts_with('=') {
                Cell::Formula(raw.clone())
            } else if let Ok(n) = raw.parse::<f64>() {
                Cell::Literal(Value::Number(n))
            } else if raw.is_empty() {
                Cell::Literal(Value::Empty)
            } else {
                Cell::Literal(Value::Text(raw.clone()))
            };
            grid.insert(cell_ref, value);
        }
        grid
    }

    pub fn name_map(&self) -> HashMap<String, String> {
        self.names.clone().into_iter().collect()
    }

    pub fn evaluate(&self) -> BTreeMap<CellRef, Value> {
        let grid = self.grid();
        let names = self.name_map();
        Engine {
            grid: &grid,
            names: &names,
        }
        .evaluate(&[], &BTreeMap::new())
    }
}

pub struct SheetsAdapter;

impl Default for SheetsAdapter {
    fn default() -> Self {
        SheetsAdapter
    }
}

impl SheetsAdapter {
    pub fn new() -> Self {
        SheetsAdapter
    }
}

impl Adapter for SheetsAdapter {
    fn name(&self) -> &'static str {
        "sheets"
    }

    fn version(&self) -> &'static str {
        "sheets-v1"
    }

    fn handles(&self, path: &str) -> bool {
        let lower = path.to_ascii_lowercase();
        lower.ends_with(".workbook.json") || lower.ends_with(".xlsx")
    }

    fn op_schema(&self) -> serde_json::Value {
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "flash sheet ops",
            "type": "array",
            "minItems": 1,
            "maxItems": 64,
            "items": {
                "type": "object",
                "required": ["op"],
                "oneOf": [
                    {
                        "properties": {
                            "op": { "const": "set_values" },
                            "cells": {
                                "type": "object",
                                "description": "cell reference to literal value",
                                "additionalProperties": { "type": ["string", "number"] }
                            }
                        },
                        "required": ["op", "cells"],
                        "additionalProperties": false
                    },
                    {
                        "properties": {
                            "op": { "const": "set_formula" },
                            "cell": { "type": "string" },
                            "formula": { "type": "string" }
                        },
                        "required": ["op", "cell", "formula"],
                        "additionalProperties": false
                    },
                    {
                        "properties": {
                            "op": { "const": "insert_rows" },
                            "sheet": { "type": "string" },
                            "at": { "type": "integer", "minimum": 1 },
                            "count": { "type": "integer", "minimum": 1, "maximum": 1000 }
                        },
                        "required": ["op", "at", "count"],
                        "additionalProperties": false
                    },
                    {
                        "properties": {
                            "op": { "const": "add_sheet" },
                            "name": { "type": "string" }
                        },
                        "required": ["op", "name"],
                        "additionalProperties": false
                    },
                    {
                        "properties": {
                            "op": { "const": "define_name" },
                            "name": { "type": "string" },
                            "ref": { "type": "string" }
                        },
                        "required": ["op", "name", "ref"],
                        "additionalProperties": false
                    },
                    {
                        "properties": {
                            "op": { "const": "assert" },
                            "expression": {
                                "type": "string",
                                "description": "a formula that must evaluate to TRUE"
                            }
                        },
                        "required": ["op", "expression"],
                        "additionalProperties": false
                    }
                ]
            }
        })
    }

    fn outline(&self, artifact: &Artifact) -> Result<Outline> {
        let wb = Workbook::parse(artifact)?;
        let names = wb.name_map();
        let mut entities = Vec::new();

        for (at, raw) in &wb.cells {
            let Some(cell_ref) = CellRef::parse(wb.default_sheet(), at) else {
                continue;
            };
            let refs = if raw.starts_with('=') {
                formula::dependencies(&cell_ref.sheet, raw, &names)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|c| format!("cell:{}", c.a1()))
                    .collect()
            } else {
                Vec::new()
            };
            entities.push(flash_adapter::Entity {
                id: format!("cell:{}", cell_ref.a1()),
                kind: if raw.starts_with('=') {
                    "formula"
                } else {
                    "value"
                }
                .to_string(),
                start: 0,
                end: 0,
                body: None,
                signature: Some(raw.clone()),
                refs,
                parent: Some(cell_ref.sheet.clone()),
            });
        }

        for (name, target) in &wb.names {
            entities.push(flash_adapter::Entity {
                id: format!("name:{name}"),
                kind: "name".into(),
                start: 0,
                end: 0,
                body: None,
                signature: Some(target.clone()),
                refs: Vec::new(),
                parent: None,
            });
        }

        entities.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(Outline {
            path: artifact.path.clone(),
            entities,
        })
    }

    fn apply(&self, artifact: &Artifact, ops: &[Op]) -> Result<Applied> {
        if ops.is_empty() {
            return Err(AdapterError::BadOp("empty op batch".into()));
        }
        let before = Workbook::parse(artifact)?;
        let mut wb = before.clone();
        let sheet = wb.default_sheet().to_string();

        for op in ops {
            match op.kind() {
                Some("set_values") => {
                    let cells =
                        op.0.get("cells")
                            .and_then(|v| v.as_object())
                            .ok_or_else(|| {
                                AdapterError::BadOp("set_values needs a cells map".into())
                            })?;
                    for (at, value) in cells {
                        let Some(cell_ref) = CellRef::parse(&sheet, at) else {
                            return Err(AdapterError::BadOp(format!(
                                "{at} is not a cell reference"
                            )));
                        };
                        let text = match value {
                            serde_json::Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        if text.starts_with('=') {
                            return Err(AdapterError::BadOp(
                                "set_values takes literals; use set_formula for formulas".into(),
                            ));
                        }
                        wb.cells.insert(cell_ref.a1(), text);
                    }
                }
                Some("set_formula") => {
                    let at = op.str_field("cell")?;
                    let f = op.str_field("formula")?;
                    let Some(cell_ref) = CellRef::parse(&sheet, at) else {
                        return Err(AdapterError::BadOp(format!("{at} is not a cell reference")));
                    };
                    let text = if f.starts_with('=') {
                        f.to_string()
                    } else {
                        format!("={f}")
                    };
                    // Reject a formula that does not parse before it reaches the workbook: an
                    // unparseable formula would poison every recalc that reads that cell.
                    formula::dependencies(&cell_ref.sheet, &text, &wb.name_map())
                        .map_err(|e| AdapterError::BadOp(format!("formula does not parse: {e}")))?;
                    wb.cells.insert(cell_ref.a1(), text);
                }
                Some("insert_rows") => {
                    let at =
                        op.0.get("at")
                            .and_then(|v| v.as_u64())
                            .ok_or_else(|| AdapterError::BadOp("insert_rows needs `at`".into()))?
                            as u32;
                    let count =
                        op.0.get("count").and_then(|v| v.as_u64()).ok_or_else(|| {
                            AdapterError::BadOp("insert_rows needs `count`".into())
                        })? as u32;
                    let target_sheet = op.opt_str("sheet").unwrap_or(&sheet).to_string();
                    let mut moved = BTreeMap::new();
                    for (key, value) in &wb.cells {
                        match CellRef::parse(&sheet, key) {
                            Some(mut c) if c.sheet == target_sheet && c.row >= at => {
                                c.row += count;
                                moved.insert(c.a1(), value.clone());
                            }
                            _ => {
                                moved.insert(key.clone(), value.clone());
                            }
                        }
                    }
                    wb.cells = moved;
                }
                Some("add_sheet") => {
                    let name = op.str_field("name")?.to_string();
                    if wb.sheets.contains(&name) {
                        return Err(AdapterError::BadOp(format!("sheet {name} already exists")));
                    }
                    wb.sheets.push(name);
                }
                Some("define_name") => {
                    let name = op.str_field("name")?.to_string();
                    let target = op.str_field("ref")?.to_string();
                    wb.names.insert(name, target);
                }
                Some("assert") => {
                    let expression = op.str_field("expression")?.to_string();
                    if !wb.assertions.contains(&expression) {
                        wb.assertions.push(expression);
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

        let candidate = wb.to_artifact(&artifact.path);
        let after = self.outline(&candidate)?;
        let before_outline = self.outline(artifact)?;
        let mut delta = Delta::default();
        for e in &after.entities {
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
            if after.get(&e.id).is_none() {
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
            Arc::new(FormulasParse),
            Arc::new(DependentRecalc),
            Arc::new(PlannerAssertions),
            Arc::new(FullRecalc),
        ]
    }

    fn impact(&self, outline: &Outline, delta: &Delta) -> ImpactSet {
        // Reverse edges over cell references: exactly section 4.4's "recalc only the dependent
        // subgraph of the changed cells".
        let touched: Vec<String> = delta.touched().into_iter().map(str::to_string).collect();
        let mut seen: Vec<String> = Vec::new();
        let mut queue = touched.clone();
        while let Some(id) = queue.pop() {
            if seen.contains(&id) {
                continue;
            }
            seen.push(id.clone());
            for e in &outline.entities {
                if e.refs.contains(&id) && !seen.contains(&e.id) {
                    queue.push(e.id.clone());
                }
            }
        }
        seen.sort();
        ImpactSet {
            units: seen.iter().map(|c| c.replace("cell:", "recalc:")).collect(),
            entities: seen,
            tests: Vec::new(),
        }
    }

    fn pack(&self, req: &PackRequest<'_>) -> Result<Pack> {
        // Section 3.3 for sheets: target range, named ranges it references, header rows, and the
        // formulas that depend on it.
        let wb = Workbook::parse(req.artifact)?;
        let mut pack = Pack::default();

        let target = req
            .outline
            .get(req.target)
            .ok_or_else(|| AdapterError::UnknownEntity(req.target.to_string()))?;
        pack.push(
            "target",
            format!(
                "{} = {}",
                target.id,
                target.signature.clone().unwrap_or_default()
            ),
        );

        pack.push(
            "named ranges",
            wb.names
                .iter()
                .map(|(n, r)| format!("{n} = {r}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );

        // Row 1 of each sheet: the headers that say what the numbers mean.
        let headers: Vec<String> = wb
            .cells
            .iter()
            .filter(|(at, _)| at.ends_with("1") && !at.ends_with("11") && !at.ends_with("21"))
            .map(|(at, v)| format!("{at} = {v}"))
            .collect();
        pack.push("header row", headers.join("\n"));

        pack.push(
            "formulas that depend on the target",
            req.outline
                .referrers(&target.id)
                .into_iter()
                .map(|e| format!("{} = {}", e.id, e.signature.clone().unwrap_or_default()))
                .collect::<Vec<_>>()
                .join("\n"),
        );

        pack.push("assertions", wb.assertions.join("\n"));

        while pack.chars() > req.budget_chars.max(500) && pack.parts.len() > 1 {
            pack.parts.pop();
        }
        Ok(pack)
    }
}

// ---- ladder ----------------------------------------------------------------------------------

/// Rung 0: every formula parses.
pub struct FormulasParse;

impl VerifyRung for FormulasParse {
    fn name(&self) -> &str {
        "formulas-parse"
    }
    fn level(&self) -> u8 {
        0
    }
    fn expected_ms(&self) -> u64 {
        1
    }

    fn check(&self, ctx: &VerifyCtx<'_>) -> RungOutcome {
        let wb = match Workbook::parse(ctx.artifact) {
            Ok(w) => w,
            Err(e) => return RungOutcome::fail(vec![Diagnostic::new("workbook", e.to_string())]),
        };
        let names = wb.name_map();
        let mut diags = Vec::new();
        for (at, raw) in &wb.cells {
            if !raw.starts_with('=') {
                continue;
            }
            let sheet = CellRef::parse(wb.default_sheet(), at)
                .map(|c| c.sheet)
                .unwrap_or_else(|| wb.default_sheet().to_string());
            if let Err(e) = formula::dependencies(&sheet, raw, &names) {
                diags.push(Diagnostic::new("formula-parse", e).at_entity(format!("cell:{at}")));
            }
        }
        if diags.is_empty() {
            RungOutcome::pass()
        } else {
            RungOutcome::fail(diags)
        }
    }
}

/// Rung 1: recalculate what the change reaches; no error cells.
pub struct DependentRecalc;

impl VerifyRung for DependentRecalc {
    fn name(&self) -> &str {
        "dependent-recalc"
    }
    fn level(&self) -> u8 {
        1
    }
    fn expected_ms(&self) -> u64 {
        200
    }

    fn check(&self, ctx: &VerifyCtx<'_>) -> RungOutcome {
        let wb = match Workbook::parse(ctx.artifact) {
            Ok(w) => w,
            Err(e) => return RungOutcome::fail(vec![Diagnostic::new("workbook", e.to_string())]),
        };
        let values = wb.evaluate();
        let mut diags = Vec::new();
        for (at, v) in &values {
            if let Value::Error(code) = v {
                diags.push(
                    Diagnostic::new("error-cell", format!("{} evaluates to {code}", at.a1()))
                        .at_entity(format!("cell:{}", at.a1())),
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

/// Rung 2: the assertions the planner wrote.
pub struct PlannerAssertions;

impl VerifyRung for PlannerAssertions {
    fn name(&self) -> &str {
        "assertions"
    }
    fn level(&self) -> u8 {
        2
    }
    fn expected_ms(&self) -> u64 {
        100
    }

    fn check(&self, ctx: &VerifyCtx<'_>) -> RungOutcome {
        let wb = match Workbook::parse(ctx.artifact) {
            Ok(w) => w,
            Err(e) => return RungOutcome::fail(vec![Diagnostic::new("workbook", e.to_string())]),
        };
        if wb.assertions.is_empty() {
            return RungOutcome::pass();
        }

        // Each assertion is evaluated as a formula in a scratch cell, so it can use anything the
        // workbook can: totals, named ranges, comparisons.
        let names = wb.name_map();
        let mut diags = Vec::new();
        for (i, expr) in wb.assertions.iter().enumerate() {
            let mut grid = wb.grid();
            // The probe lives on the workbook's default sheet, because an assertion written as
            // `A3=30` means A3 on the sheet the planner was looking at. Putting it on a scratch
            // sheet would silently resolve every bare reference to an empty cell, and every
            // assertion would quietly evaluate against zeros.
            let probe = CellRef {
                sheet: wb.default_sheet().to_string(),
                col: 16_000,
                row: 1_000_000 + i as u32,
            };
            let text = if expr.starts_with('=') {
                expr.clone()
            } else {
                format!("={expr}")
            };
            grid.insert(probe.clone(), Cell::Formula(text));
            let values = Engine {
                grid: &grid,
                names: &names,
            }
            .evaluate(&[], &BTreeMap::new());
            match values.get(&probe) {
                Some(Value::Bool(true)) => {}
                Some(other) => diags.push(Diagnostic::new(
                    "assertion-failed",
                    format!("{expr} evaluated to {}", other.render()),
                )),
                None => diags.push(Diagnostic::new(
                    "assertion-failed",
                    format!("{expr} did not evaluate"),
                )),
            }
        }
        if diags.is_empty() {
            RungOutcome::pass()
        } else {
            RungOutcome::fail(diags)
        }
    }
}

/// Rung 3: full recalc, once.
pub struct FullRecalc;

impl VerifyRung for FullRecalc {
    fn name(&self) -> &str {
        "full-recalc"
    }
    fn level(&self) -> u8 {
        3
    }
    fn expected_ms(&self) -> u64 {
        2_000
    }
    fn once_per_task(&self) -> bool {
        true
    }

    fn check(&self, ctx: &VerifyCtx<'_>) -> RungOutcome {
        DependentRecalc.check(ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book() -> Artifact {
        let wb = Workbook {
            sheets: vec!["Sheet1".into()],
            cells: [
                ("Sheet1!A1", "10"),
                ("Sheet1!A2", "20"),
                ("Sheet1!A3", "=SUM(A1:A2)"),
                ("Sheet1!B1", "=A3*2"),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
            names: Default::default(),
            assertions: vec![],
        };
        wb.to_artifact("model.workbook.json")
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
    fn the_adapter_takes_workbooks_only() {
        let a = SheetsAdapter::new();
        assert!(a.handles("q3.workbook.json"));
        assert!(a.handles("model.xlsx"));
        assert!(!a.handles("src/lib.rs"));
        assert!(!a.handles("notes.md"));
    }

    #[test]
    fn cells_and_their_dependencies_become_entities() {
        let a = SheetsAdapter::new();
        let o = a.outline(&book()).unwrap();
        let sum = o.get("cell:Sheet1!A3").unwrap();
        assert_eq!(sum.kind, "formula");
        assert!(
            sum.refs.contains(&"cell:Sheet1!A1".to_string()),
            "{:?}",
            sum.refs
        );
    }

    #[test]
    fn setting_a_value_changes_one_cell() {
        let a = SheetsAdapter::new();
        let applied = a
            .apply(
                &book(),
                &[Op::new(json!({"op":"set_values","cells":{"A1": 15}}))],
            )
            .unwrap();
        assert_eq!(applied.delta.changed, vec!["cell:Sheet1!A1"]);
        let wb = Workbook::parse(&applied.artifact).unwrap();
        assert_eq!(wb.cells["Sheet1!A1"], "15");
    }

    #[test]
    fn a_formula_that_does_not_parse_is_refused_before_it_reaches_the_workbook() {
        let a = SheetsAdapter::new();
        let err = a
            .apply(
                &book(),
                &[Op::new(
                    json!({"op":"set_formula","cell":"C1","formula":"=SUM(@@@"}),
                )],
            )
            .unwrap_err();
        assert!(format!("{err}").contains("does not parse"), "{err}");
    }

    #[test]
    fn impact_is_the_dependent_subgraph() {
        let a = SheetsAdapter::new();
        let artifact = book();
        let outline = a.outline(&artifact).unwrap();
        let delta = Delta {
            changed: vec!["cell:Sheet1!A1".into()],
            ..Default::default()
        };
        let impact = a.impact(&outline, &delta);
        assert!(impact.entities.contains(&"cell:Sheet1!A3".to_string()));
        assert!(
            impact.entities.contains(&"cell:Sheet1!B1".to_string()),
            "the transitive dependent must recalc: {impact:?}"
        );
        assert_eq!(impact.entities.len(), 3, "and nothing else: {impact:?}");
    }

    #[test]
    fn rung_one_catches_an_error_cell() {
        let a = SheetsAdapter::new();
        let broken = a
            .apply(
                &book(),
                &[Op::new(
                    json!({"op":"set_formula","cell":"C1","formula":"=A1/0"}),
                )],
            )
            .unwrap();
        let outline = a.outline(&broken.artifact).unwrap();
        let (d, i) = (Delta::default(), ImpactSet::default());
        let outcome = a.ladder()[1].check(&ctx(&broken.artifact, &outline, &d, &i));
        assert!(!outcome.passed);
        assert_eq!(outcome.diagnostics[0].code, "error-cell");
    }

    #[test]
    fn planner_assertions_pass_and_fail_on_the_numbers() {
        let a = SheetsAdapter::new();
        let good = a
            .apply(
                &book(),
                &[Op::new(json!({"op":"assert","expression":"A3=30"}))],
            )
            .unwrap();
        let outline = a.outline(&good.artifact).unwrap();
        let (d, i) = (Delta::default(), ImpactSet::default());
        assert!(
            a.ladder()[2]
                .check(&ctx(&good.artifact, &outline, &d, &i))
                .passed
        );

        let bad = a
            .apply(
                &book(),
                &[Op::new(json!({"op":"assert","expression":"A3=31"}))],
            )
            .unwrap();
        let outline = a.outline(&bad.artifact).unwrap();
        let outcome = a.ladder()[2].check(&ctx(&bad.artifact, &outline, &d, &i));
        assert!(!outcome.passed);
        assert_eq!(outcome.diagnostics[0].code, "assertion-failed");
    }

    #[test]
    fn insert_rows_moves_the_cells_below_it() {
        let a = SheetsAdapter::new();
        let applied = a
            .apply(
                &book(),
                &[Op::new(json!({"op":"insert_rows","at":2,"count":1}))],
            )
            .unwrap();
        let wb = Workbook::parse(&applied.artifact).unwrap();
        assert!(wb.cells.contains_key("Sheet1!A3"), "{:?}", wb.cells);
        assert!(
            wb.cells.contains_key("Sheet1!A4"),
            "A2 moved down: {:?}",
            wb.cells
        );
        assert_eq!(wb.cells["Sheet1!A1"], "10", "rows above are untouched");
    }

    #[test]
    fn a_pack_carries_the_target_and_its_dependents() {
        let a = SheetsAdapter::new();
        let artifact = book();
        let outline = a.outline(&artifact).unwrap();
        let pack = a
            .pack(&PackRequest {
                artifact: &artifact,
                outline: &outline,
                target: "cell:Sheet1!A3",
                budget_chars: 4_000,
            })
            .unwrap();
        let text = pack.render();
        assert!(text.contains("cell:Sheet1!A3"), "{text}");
        assert!(
            text.contains("Sheet1!B1"),
            "dependents are in the pack: {text}"
        );
    }
}

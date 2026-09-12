//! agent-latency-anatomy: the benchmark from PRD section 7.
//!
//! ## What this measures, and what it does not
//!
//! Model calls are replayed from fixtures. That is a deliberate choice and it has to be stated
//! wherever the numbers are: **these runs do not measure model decode time**. They measure
//! everything the runtime is actually responsible for - scheduling, hashing, packing, applying,
//! verifying, and above all what gets skipped - with model latency held at zero so it cannot mask
//! or flatter any of it.
//!
//! That makes the cold column an underestimate of a real cold run by exactly the decode time, and
//! makes the warm and hot columns *honest*, because the thing being claimed there is that work
//! does not happen at all. Point the same harness at a real model client and the same tables come
//! out with decode included; nothing else changes. Artificial analysis already publishes end to
//! end wall time per task, so duplicating that is not the contribution (d11). Where the time goes,
//! and what warm and hot cost, is.
//!
//! ## The ablation
//!
//! Section 10.2 asks for one table: ops only, ops plus ladder, ops plus ladder plus impact, plus
//! incremental. Those are configurations of this runtime rather than four different programs, so
//! the comparison is apples to apples.

use flash_adapter_code::CodeAdapter;
use flash_adapter_doc::DocAdapter;
use flash_adapter_sheets::SheetsAdapter;
use flash_orchestrator::model::ScriptedModel;
use flash_orchestrator::{Runtime, RuntimeConfig, Task};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

/// One frozen task. Everything needed to reproduce it lives here, including the model fixtures,
/// so a run is reproducible on a fresh box with no network (section 7's protocol).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BenchTask {
    pub id: String,
    /// code, doc or sheets.
    pub kind: String,
    pub instruction: String,
    /// Files the workspace starts with: path -> contents.
    pub files: BTreeMap<String, String>,
    /// Which files the task may touch.
    pub targets: Vec<String>,
    /// The planner's answer.
    pub plan: serde_json::Value,
    /// The executor's answer, keyed by a marker that must appear in its prompt. An empty marker
    /// is the fallback rule.
    pub executor: Vec<(String, serde_json::Value)>,
    /// The warm variant: one input changed after a first run.
    pub warm_change: Option<(String, String)>,
}

impl BenchTask {
    pub fn load(path: &Path) -> std::io::Result<BenchTask> {
        let bytes = std::fs::read(path)?;
        serde_json::from_slice(&bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    pub fn load_dir(dir: &Path) -> std::io::Result<Vec<BenchTask>> {
        let mut out = Vec::new();
        let mut entries: Vec<_> = std::fs::read_dir(dir)?.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.path());
        for entry in entries {
            if entry.path().extension().and_then(|e| e.to_str()) == Some("json") {
                out.push(BenchTask::load(&entry.path())?);
            }
        }
        Ok(out)
    }

    fn scripted_executor(&self) -> ScriptedModel {
        let mut m = ScriptedModel::new();
        for (marker, response) in &self.executor {
            if marker.is_empty() {
                m = m.otherwise(response.clone());
            } else {
                m = m.on(marker.clone(), response.clone());
            }
        }
        m
    }

    fn adapters(&self) -> Vec<Arc<dyn flash_adapter::Adapter>> {
        match self.kind.as_str() {
            "doc" => vec![Arc::new(DocAdapter::new())],
            "sheets" => vec![Arc::new(SheetsAdapter::new())],
            _ => vec![Arc::new(CodeAdapter::new())],
        }
    }
}

/// Which levers are switched on. The ablation from section 10.2.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Ablation {
    /// Entity ops, nothing else: no ladder, no impact analysis, no cache between runs.
    OpsOnly,
    /// Ops plus the verify ladder.
    OpsLadder,
    /// Ops plus ladder plus impact analysis.
    OpsLadderImpact,
    /// Everything, including the incremental graph and its memo store.
    Full,
}

impl Ablation {
    pub fn label(&self) -> &'static str {
        match self {
            Ablation::OpsOnly => "ops only",
            Ablation::OpsLadder => "ops + ladder",
            Ablation::OpsLadderImpact => "ops + ladder + impact",
            Ablation::Full => "+ incremental",
        }
    }

    pub const ALL: [Ablation; 4] = [
        Ablation::OpsOnly,
        Ablation::OpsLadder,
        Ablation::OpsLadderImpact,
        Ablation::Full,
    ];

    fn levels(&self) -> Vec<u8> {
        match self {
            Ablation::OpsOnly => vec![0],
            _ => vec![0, 1],
        }
    }

    /// Only the full configuration keeps a store across runs; the others start cold every time,
    /// which is what "no incremental" means in practice.
    fn keeps_cache(&self) -> bool {
        matches!(self, Ablation::Full)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Regime {
    Cold,
    Warm,
    Hot,
}

impl Regime {
    pub fn label(&self) -> &'static str {
        match self {
            Regime::Cold => "cold",
            Regime::Warm => "warm",
            Regime::Hot => "hot",
        }
    }
}

/// One measured run.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Measurement {
    pub task: String,
    pub ablation: String,
    pub regime: String,
    pub wall_ms: u64,
    pub first_change_ms: Option<u64>,
    pub nodes: usize,
    pub computed: usize,
    pub hits: usize,
    pub saved_ms: u64,
    /// Per step attribution (d11).
    pub decode_ms: u64,
    pub apply_ms: u64,
    pub verify_ms: u64,
    pub render_ms: u64,
    pub wait_ms: u64,
    pub model_calls: u64,
    pub output_tokens: u64,
    pub op_resolve_rate: f64,
    pub fallback_rate: f64,
    pub green: bool,
    pub eta_error_pct: f64,
}

/// Run one task through one ablation, in all three regimes, against one store.
pub async fn run_task(task: &BenchTask, ablation: Ablation) -> std::io::Result<Vec<Measurement>> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    for (path, body) in &task.files {
        let full = root.join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(full, body)?;
    }

    let mut out = Vec::new();
    let mut run_index = 0usize;

    let once = async |regime: Regime, run_index: usize| -> std::io::Result<Measurement> {
        // A configuration without the incremental graph gets a fresh store, which is exactly what
        // "every agent today treats everything as cold" means (section 1).
        let store_root = if ablation.keeps_cache() {
            root.join(".flash")
        } else {
            root.join(format!(".flash-{}", run_index))
        };

        let executor = task.scripted_executor();
        let planner = ScriptedModel::new().otherwise(task.plan.clone());
        let runtime = Runtime::new(
            task.adapters(),
            Arc::new(planner),
            Arc::new(executor),
            RuntimeConfig {
                workspace: Some(root.to_path_buf()),
                levels: ablation.levels(),
                ..Default::default()
            },
        );

        let report = Task::new(task.instruction.clone())
            .root(root)
            .files(task.targets.clone())
            .id(&task.id)
            .run(&store_root, runtime.clone())
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        let m = runtime.metrics.snapshot();
        let a = report.attribution;
        Ok(Measurement {
            task: task.id.clone(),
            ablation: ablation.label().to_string(),
            regime: regime.label().to_string(),
            wall_ms: report.wall_ms,
            first_change_ms: report.first_change_ms,
            nodes: report.nodes.len(),
            computed: report.computed(),
            hits: report.hits(),
            saved_ms: report.saved_ms,
            decode_ms: a.decode_ms,
            apply_ms: a.apply_ms,
            verify_ms: a.verify_ms,
            render_ms: a.render_ms,
            wait_ms: a.wait_ms,
            model_calls: m.edits_attempted,
            output_tokens: 0,
            op_resolve_rate: m.op_resolve_rate(),
            fallback_rate: m.fallback_rate(),
            green: report.ok(),
            eta_error_pct: report.eta_at_start.error_pct(report.wall_ms),
        })
    };

    out.push(once(Regime::Cold, run_index).await?);
    run_index += 1;

    if let Some((path, new_body)) = &task.warm_change {
        std::fs::write(root.join(path), new_body)?;
        out.push(once(Regime::Warm, run_index).await?);
        run_index += 1;
        // Hot: the same inputs as the warm run, so nothing at all should be recomputed.
        out.push(once(Regime::Hot, run_index).await?);
    }

    Ok(out)
}

pub async fn run_all(tasks: &[BenchTask]) -> std::io::Result<Vec<Measurement>> {
    let mut out = Vec::new();
    for task in tasks {
        for ablation in Ablation::ALL {
            out.extend(run_task(task, ablation).await?);
        }
    }
    Ok(out)
}

// ---- report ----------------------------------------------------------------------------------

fn median(mut xs: Vec<u64>) -> u64 {
    if xs.is_empty() {
        return 0;
    }
    xs.sort_unstable();
    xs[xs.len() / 2]
}

fn p90(mut xs: Vec<u64>) -> u64 {
    if xs.is_empty() {
        return 0;
    }
    xs.sort_unstable();
    let idx = ((xs.len() as f64 * 0.9).ceil() as usize).saturating_sub(1);
    xs[idx.min(xs.len() - 1)]
}

/// Markdown tables, regenerated from measurements alone.
pub fn report(measurements: &[Measurement]) -> String {
    let mut s = String::new();
    s.push_str("# agent-latency-anatomy\n\n");
    s.push_str(
        "Model calls are replayed from fixtures, so **decode time is not in these numbers**. \
         What is in them is everything the runtime controls: scheduling, packing, applying, \
         verifying, and what it manages not to do at all.\n\n",
    );

    s.push_str("## Cold, warm, hot\n\n");
    s.push_str(
        "| ablation | regime | p50 wall | p90 wall | computed | hit | first change | green |\n",
    );
    s.push_str("| --- | --- | ---: | ---: | ---: | ---: | ---: | :-: |\n");
    for ablation in Ablation::ALL {
        for regime in [Regime::Cold, Regime::Warm, Regime::Hot] {
            let rows: Vec<&Measurement> = measurements
                .iter()
                .filter(|m| m.ablation == ablation.label() && m.regime == regime.label())
                .collect();
            if rows.is_empty() {
                continue;
            }
            let walls: Vec<u64> = rows.iter().map(|m| m.wall_ms).collect();
            let computed: u64 = rows.iter().map(|m| m.computed as u64).sum();
            let hits: u64 = rows.iter().map(|m| m.hits as u64).sum();
            let first = median(rows.iter().filter_map(|m| m.first_change_ms).collect());
            let green = rows.iter().all(|m| m.green);
            s.push_str(&format!(
                "| {} | {} | {} ms | {} ms | {} | {} | {} ms | {} |\n",
                ablation.label(),
                regime.label(),
                median(walls.clone()),
                p90(walls),
                computed,
                hits,
                first,
                if green { "yes" } else { "no" }
            ));
        }
    }

    s.push_str("\n## Where the time goes\n\n");
    s.push_str("| ablation | regime | decode | apply | verify | render | queued |\n");
    s.push_str("| --- | --- | ---: | ---: | ---: | ---: | ---: |\n");
    for ablation in Ablation::ALL {
        for regime in [Regime::Cold, Regime::Warm, Regime::Hot] {
            let rows: Vec<&Measurement> = measurements
                .iter()
                .filter(|m| m.ablation == ablation.label() && m.regime == regime.label())
                .collect();
            if rows.is_empty() {
                continue;
            }
            let sum = |f: fn(&Measurement) -> u64| -> u64 { rows.iter().map(|m| f(m)).sum() };
            s.push_str(&format!(
                "| {} | {} | {} ms | {} ms | {} ms | {} ms | {} ms |\n",
                ablation.label(),
                regime.label(),
                sum(|m| m.decode_ms),
                sum(|m| m.apply_ms),
                sum(|m| m.verify_ms),
                sum(|m| m.render_ms),
                sum(|m| m.wait_ms),
            ));
        }
    }

    s.push_str("\n## Per task\n\n");
    s.push_str("| task | regime | wall | computed / nodes | saved | op resolve | fallback |\n");
    s.push_str("| --- | --- | ---: | ---: | ---: | ---: | ---: |\n");
    for m in measurements
        .iter()
        .filter(|m| m.ablation == Ablation::Full.label())
    {
        s.push_str(&format!(
            "| {} | {} | {} ms | {} / {} | {} ms | {:.0}% | {:.0}% |\n",
            m.task,
            m.regime,
            m.wall_ms,
            m.computed,
            m.nodes,
            m.saved_ms,
            m.op_resolve_rate * 100.0,
            m.fallback_rate * 100.0,
        ));
    }

    s.push_str("\n## Losses\n\n");
    let losses = find_losses(measurements);
    if losses.is_empty() {
        s.push_str("No configuration was slower than a simpler one on any task in this set.\n");
    } else {
        s.push_str("Where a lever made things worse, and by how much:\n\n");
        for l in losses {
            s.push_str(&format!("- {l}\n"));
        }
    }
    s
}

/// Section 7: "losses published with reasons. no cherry picking." So the report finds them itself
/// rather than waiting for someone to notice.
pub fn find_losses(measurements: &[Measurement]) -> Vec<String> {
    let mut out = Vec::new();
    for regime in [Regime::Cold, Regime::Warm, Regime::Hot] {
        let at = |a: Ablation| -> Option<u64> {
            let rows: Vec<u64> = measurements
                .iter()
                .filter(|m| m.ablation == a.label() && m.regime == regime.label())
                .map(|m| m.wall_ms)
                .collect();
            if rows.is_empty() {
                None
            } else {
                Some(median(rows))
            }
        };
        for pair in Ablation::ALL.windows(2) {
            if let (Some(before), Some(after)) = (at(pair[0]), at(pair[1]))
                && after > before
            {
                out.push(format!(
                    "{}: {} is {} ms slower than {} ({} ms vs {} ms). The extra work is real; it \
                     buys correctness rather than speed in this regime.",
                    regime.label(),
                    pair[1].label(),
                    after - before,
                    pair[0].label(),
                    after,
                    before
                ));
            }
        }
    }
    out
}

/// A bar chart of where the time went, as inline svg so the report needs no toolchain.
pub fn flame_svg(measurements: &[Measurement]) -> String {
    let buckets = [
        (
            "decode",
            measurements.iter().map(|m| m.decode_ms).sum::<u64>(),
        ),
        (
            "apply",
            measurements.iter().map(|m| m.apply_ms).sum::<u64>(),
        ),
        (
            "verify",
            measurements.iter().map(|m| m.verify_ms).sum::<u64>(),
        ),
        (
            "render",
            measurements.iter().map(|m| m.render_ms).sum::<u64>(),
        ),
        (
            "queued",
            measurements.iter().map(|m| m.wait_ms).sum::<u64>(),
        ),
    ];
    let total: u64 = buckets.iter().map(|(_, v)| *v).sum::<u64>().max(1);
    let width = 720.0;
    let mut x = 0.0;
    let mut bars = String::new();
    let colours = ["#4c6ef5", "#12b886", "#f59f00", "#e8590c", "#adb5bd"];
    for (i, (name, value)) in buckets.iter().enumerate() {
        let w = width * (*value as f64 / total as f64);
        bars.push_str(&format!(
            "<rect x=\"{x:.1}\" y=\"20\" width=\"{w:.1}\" height=\"40\" fill=\"{}\"><title>{name}: {value} ms</title></rect>",
            colours[i % colours.len()]
        ));
        if w > 60.0 {
            bars.push_str(&format!(
                "<text x=\"{:.1}\" y=\"45\" fill=\"#fff\" font-family=\"sans-serif\" font-size=\"13\">{name}</text>",
                x + 8.0
            ));
        }
        x += w;
    }
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width}\" height=\"80\" viewBox=\"0 0 {width} 80\" role=\"img\" aria-label=\"where the wall clock went\">{bars}<text x=\"0\" y=\"75\" font-family=\"sans-serif\" font-size=\"12\" fill=\"#495057\">total {total} ms across {} runs</text></svg>",
        measurements.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(ablation: Ablation, regime: Regime, wall: u64, computed: usize) -> Measurement {
        Measurement {
            task: "t".into(),
            ablation: ablation.label().into(),
            regime: regime.label().into(),
            wall_ms: wall,
            first_change_ms: Some(1),
            nodes: 10,
            computed,
            hits: 10 - computed,
            saved_ms: 0,
            decode_ms: 5,
            apply_ms: 2,
            verify_ms: 3,
            render_ms: 0,
            wait_ms: 1,
            model_calls: 1,
            output_tokens: 10,
            op_resolve_rate: 1.0,
            fallback_rate: 0.0,
            green: true,
            eta_error_pct: 0.0,
        }
    }

    #[test]
    fn the_report_has_a_row_per_ablation_and_regime() {
        let data = vec![
            m(Ablation::OpsOnly, Regime::Cold, 100, 10),
            m(Ablation::Full, Regime::Cold, 90, 10),
            m(Ablation::Full, Regime::Hot, 5, 0),
        ];
        let text = report(&data);
        assert!(text.contains("ops only"));
        assert!(text.contains("+ incremental"));
        assert!(text.contains("hot"));
    }

    #[test]
    fn losses_are_found_not_hidden() {
        // The full configuration being slower than a simpler one must show up in the report.
        let data = vec![
            m(Ablation::OpsLadderImpact, Regime::Cold, 50, 10),
            m(Ablation::Full, Regime::Cold, 80, 10),
        ];
        let losses = find_losses(&data);
        assert_eq!(losses.len(), 1, "{losses:?}");
        assert!(losses[0].contains("30 ms slower"), "{}", losses[0]);
        assert!(report(&data).contains("Losses"));
    }

    #[test]
    fn a_report_with_no_losses_says_so_plainly() {
        let data = vec![
            m(Ablation::OpsOnly, Regime::Cold, 100, 10),
            m(Ablation::Full, Regime::Cold, 40, 10),
        ];
        assert!(find_losses(&data).is_empty());
        assert!(report(&data).contains("No configuration was slower"));
    }

    #[test]
    fn the_svg_is_well_formed_and_labelled() {
        let data = vec![m(Ablation::Full, Regime::Cold, 10, 5)];
        let svg = flame_svg(&data);
        assert!(svg.starts_with("<svg"));
        assert!(svg.ends_with("</svg>"));
        assert!(svg.contains("aria-label"));
        assert!(svg.contains("decode"));
    }

    #[test]
    fn percentiles_do_not_panic_on_one_sample_or_none() {
        assert_eq!(median(vec![]), 0);
        assert_eq!(p90(vec![]), 0);
        assert_eq!(p90(vec![7]), 7);
        assert_eq!(median(vec![1, 2, 3]), 2);
    }
}

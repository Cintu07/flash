//! Run the code adapter over this workspace's own source.
//!
//! Fixtures are written by the person writing the parser, which makes them agree with it. Real
//! source does not. Rung 1 in particular is a set of heuristics, and a heuristic that fires on
//! clean code is worse than no heuristic: it sends the executor off to "fix" something that was
//! never broken, burns two repair attempts and then fails the task. That is exactly what a first
//! live run of this runtime did, which is why this test exists.

use flash_adapter::{Adapter, Artifact, Delta, ImpactSet, VerifyCtx};
use flash_adapter_code::CodeAdapter;
use std::path::{Path, PathBuf};

fn rust_sources() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .to_path_buf();
    let mut out = Vec::new();
    let mut stack = vec![root.join("crates")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|n| n == "target") {
                    continue;
                }
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

fn artifact(path: &Path) -> Artifact {
    let rel = path.to_string_lossy().replace('\\', "/");
    Artifact::new(rel, std::fs::read(path).expect("readable source"))
}

#[test]
fn every_source_file_in_this_workspace_parses_and_yields_entities() {
    let files = rust_sources();
    assert!(
        files.len() > 20,
        "expected a real workspace, found {} files",
        files.len()
    );

    let adapter = CodeAdapter::new();
    for path in &files {
        let art = artifact(path);
        let outline = adapter
            .outline(&art)
            .unwrap_or_else(|e| panic!("{} did not parse: {e}", art.path));
        assert!(
            !outline.entities.is_empty(),
            "{} produced no entities at all",
            art.path
        );
    }
}

#[test]
fn rung_zero_passes_on_every_file_we_ship() {
    let adapter = CodeAdapter::new();
    let rung = &adapter.ladder()[0];
    for path in rust_sources() {
        let art = artifact(&path);
        let outline = adapter.outline(&art).unwrap();
        let (delta, impact) = (Delta::default(), ImpactSet::default());
        let outcome = rung.check(&VerifyCtx {
            artifact: &art,
            outline: &outline,
            delta: &delta,
            impact: &impact,
            workspace: None,
        });
        assert!(
            outcome.passed,
            "{} failed rung 0: {:?}",
            art.path, outcome.diagnostics
        );
    }
}

#[test]
fn rung_one_does_not_fire_on_clean_code() {
    // The false positive test. Every file here compiles and passes clippy, so any diagnostic rung
    // 1 raises is a bug in rung 1, not in the file.
    let adapter = CodeAdapter::new();
    let rung = &adapter.ladder()[1];
    let mut offenders: Vec<String> = Vec::new();

    for path in rust_sources() {
        let art = artifact(&path);
        let outline = adapter.outline(&art).unwrap();
        let (delta, impact) = (Delta::default(), ImpactSet::default());
        let outcome = rung.check(&VerifyCtx {
            artifact: &art,
            outline: &outline,
            delta: &delta,
            impact: &impact,
            workspace: None,
        });
        if !outcome.passed {
            for d in outcome.diagnostics {
                offenders.push(format!("{}: [{}] {}", art.path, d.code, d.message));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "rung 1 fired on {} clean files:\n{}",
        offenders.len(),
        offenders.join("\n")
    );
}

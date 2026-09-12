//! The code verify ladder (PRD section 5).
//!
//! | rung | check | time |
//! |---|---|---|
//! | 0 | reparse the file | ~1 ms |
//! | 1 | structural lint on the file | ~50 ms |
//! | 2 | incremental typecheck | 2 to 10 s |
//! | 3 | impacted tests only | 1 to 8 s |
//! | 4 | full suite, once per task | end of task |
//!
//! Two things here are not in the table and matter more than it does.
//!
//! **A rung that cannot run is not a pass.** If `cargo` is not on the path, rung 2 reports
//! *unavailable*, which the orchestrator must treat as "unverified" rather than "fine". The
//! tempting alternative - skip it and carry on - quietly turns the ladder into a syntax checker
//! and writes unverified work into a shared cache.
//!
//! **Rung 1 exists to catch what rung 2 would catch, sooner.** Its value is entirely in the
//! ratio of what it catches to what it costs, which is the paper's claim 2. So it only contains
//! checks that are genuinely local: no attempt at type inference, no pretend name resolution.

use crate::lang::LangSpec;
use crate::symbols;
use flash_adapter::{Diagnostic, RungOutcome, VerifyCtx, VerifyRung};
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

/// Does `haystack` use `needle` as a whole identifier?
///
/// Not a substring test: `parse` must not match inside `parser`, or deleting one function reports
/// every similarly named neighbour as broken.
fn mentions_word(haystack: &str, needle: &str) -> bool {
    let bytes = haystack.as_bytes();
    let mut from = 0usize;
    while let Some(found) = haystack[from..].find(needle) {
        let at = from + found;
        let before_ok = at == 0 || !is_ident_byte(bytes[at - 1]);
        let after = at + needle.len();
        let after_ok = after >= bytes.len() || !is_ident_byte(bytes[after]);
        if before_ok && after_ok {
            return true;
        }
        from = at + needle.len().max(1);
        if from >= haystack.len() {
            break;
        }
    }
    false
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Line number (1-based) of a byte offset.
fn line_of(src: &str, byte: usize) -> u32 {
    (src[..byte.min(src.len())].matches('\n').count() + 1) as u32
}

/// Rung 0: does it still parse?
pub struct Reparse {
    pub spec: &'static LangSpec,
}

impl VerifyRung for Reparse {
    fn name(&self) -> &str {
        "reparse"
    }
    fn level(&self) -> u8 {
        0
    }
    fn expected_ms(&self) -> u64 {
        1
    }

    fn check(&self, ctx: &VerifyCtx<'_>) -> RungOutcome {
        let tree = match symbols::parse(self.spec, ctx.artifact) {
            Ok(t) => t,
            Err(e) => {
                return RungOutcome::fail(vec![Diagnostic::new("parse-failed", e.to_string())]);
            }
        };
        let errors = symbols::syntax_errors(&tree);
        if errors.is_empty() {
            return RungOutcome::pass();
        }
        let src = ctx.artifact.text().to_string();
        RungOutcome::fail(
            errors
                .iter()
                .map(|(start, _, what)| {
                    Diagnostic::new("syntax", what.clone()).at_line(line_of(&src, *start))
                })
                .collect(),
        )
    }
}

/// Rung 1: structural lint. Local facts only, no inference.
pub struct Lint {
    pub spec: &'static LangSpec,
}

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
        let src = ctx.artifact.text().to_string();
        let mut diags = Vec::new();

        // Two definitions with the same id. The parser accepts it; the compiler will not.
        let mut seen: HashMap<&str, usize> = HashMap::new();
        for e in &ctx.outline.entities {
            if e.kind == "import" {
                continue;
            }
            *seen.entry(e.id.as_str()).or_insert(0) += 1;
        }
        for (id, n) in seen {
            if n > 1 {
                diags.push(
                    Diagnostic::new("duplicate-definition", format!("{id} is defined {n} times"))
                        .at_entity(id),
                );
            }
        }

        // A reference to something this edit removed. This is the single most common way an
        // entity op breaks a file, and it costs nothing to see.
        //
        // The match is textual on purpose. The outline handed to this rung is the one *after* the
        // edit, so the deleted entity is not in it and no surviving entity has a reference edge
        // pointing at it any more - which meant the first version of this check could never fire
        // outside a unit test that hand-injected the edge. What is still true after the deletion
        // is that the caller's source spells the name.
        for removed in &ctx.delta.removed {
            let simple = removed
                .split_once(':')
                .map(|(_, r)| r)
                .unwrap_or(removed)
                .rsplit(self.spec.sep)
                .next()
                .unwrap_or("");
            if simple.is_empty() {
                continue;
            }
            for e in &ctx.outline.entities {
                if e.kind == "import" {
                    continue;
                }
                let (bs, be) = e.body.unwrap_or((e.start, e.end));
                let body = src.get(bs.min(src.len())..be.min(src.len())).unwrap_or("");
                if mentions_word(body, simple) {
                    diags.push(
                        Diagnostic::new(
                            "dangling-reference",
                            format!(
                                "{} still refers to {removed}, which this edit removed",
                                e.id
                            ),
                        )
                        .at_entity(e.id.clone())
                        .at_line(line_of(&src, e.start)),
                    );
                }
            }
        }

        // There is deliberately no unused-import check here.
        //
        // The obvious version - flag an import whose tail never appears again in the file - fires
        // on two things that are completely normal: `pub use` re-exports, whose entire purpose is
        // to be unused locally, and trait imports like `use std::io::Write`, which are used only
        // through method call syntax that never spells the trait's name. Running this rung over
        // this repo's own source produced ten such diagnostics and zero true positives.
        //
        // A false positive here is not a cosmetic annoyance. The executor is handed the
        // diagnostic, spends two repair attempts "fixing" code that was never broken, and the
        // task fails. Rung 2 is a real compiler and reports unused imports correctly; a rung 1
        // heuristic that cannot distinguish a re-export from dead code has no business guessing.

        if diags.is_empty() {
            RungOutcome::pass()
        } else {
            RungOutcome::fail(diags)
        }
    }
}

/// How an external tool is invoked. Kept as data so a test can point it at a stub and so a repo
/// can override the command without a code change.
#[derive(Clone, Debug)]
pub struct ToolSpec {
    pub program: String,
    pub args: Vec<String>,
    /// Parse the output as cargo's json diagnostics.
    pub cargo_json: bool,
}

impl ToolSpec {
    pub fn new(program: &str, args: &[&str]) -> Self {
        ToolSpec {
            program: program.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            cargo_json: false,
        }
    }

    pub fn cargo_json(mut self) -> Self {
        self.cargo_json = true;
        self
    }

    pub fn rust_typecheck() -> Self {
        ToolSpec::new("cargo", &["check", "--message-format=json", "--quiet"]).cargo_json()
    }

    pub fn rust_test() -> Self {
        ToolSpec::new("cargo", &["test", "--quiet"])
    }
}

fn run_tool(
    spec: &ToolSpec,
    workspace: &Path,
    extra: &[String],
) -> std::io::Result<std::process::Output> {
    Command::new(&spec.program)
        .args(&spec.args)
        .args(extra)
        .current_dir(workspace)
        .output()
}

/// Turn cargo's json output into structured diagnostics. Never raw output (section 5).
fn parse_cargo_json(stdout: &str) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("reason").and_then(|r| r.as_str()) != Some("compiler-message") {
            continue;
        }
        let Some(msg) = v.get("message") else {
            continue;
        };
        let level = msg.get("level").and_then(|l| l.as_str()).unwrap_or("");
        if level != "error" {
            continue;
        }
        let code = msg
            .get("code")
            .and_then(|c| c.get("code"))
            .and_then(|c| c.as_str())
            .unwrap_or("error")
            .to_string();
        let text = msg
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();
        let line_no = msg
            .get("spans")
            .and_then(|s| s.as_array())
            .and_then(|a| a.first())
            .and_then(|s| s.get("line_start"))
            .and_then(|l| l.as_u64());
        let mut d = Diagnostic::new(code, text);
        if let Some(l) = line_no {
            d = d.at_line(l as u32);
        }
        out.push(d);
    }
    out
}

/// Rung 2: hand the file to the real toolchain.
pub struct Typecheck {
    pub tool: ToolSpec,
    pub expected_ms: u64,
}

impl VerifyRung for Typecheck {
    fn name(&self) -> &str {
        "typecheck"
    }
    fn level(&self) -> u8 {
        2
    }
    fn expected_ms(&self) -> u64 {
        self.expected_ms
    }

    fn check(&self, ctx: &VerifyCtx<'_>) -> RungOutcome {
        let Some(ws) = ctx.workspace else {
            return RungOutcome::unavailable("no workspace directory for the typecheck rung");
        };
        match run_tool(&self.tool, ws, &[]) {
            Err(e) => RungOutcome::unavailable(format!("{} could not run: {e}", self.tool.program)),
            Ok(out) => {
                if out.status.success() {
                    return RungOutcome::pass();
                }
                let stdout = String::from_utf8_lossy(&out.stdout);
                let mut diags = if self.tool.cargo_json {
                    parse_cargo_json(&stdout)
                } else {
                    Vec::new()
                };
                if diags.is_empty() {
                    // Never hand back raw tool output; summarise it as one item.
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    let first = stderr
                        .lines()
                        .find(|l| l.contains("error"))
                        .unwrap_or("typecheck failed")
                        .trim()
                        .to_string();
                    diags.push(Diagnostic::new("typecheck", first));
                }
                RungOutcome::fail(diags)
            }
        }
    }
}

/// Rung 3: only the tests the change can reach (d7).
pub struct ImpactedTests {
    pub tool: ToolSpec,
    pub expected_ms: u64,
    /// Above this many impacted tests, running them one by one costs more than the suite does.
    pub fallback_to_all_above: usize,
}

impl VerifyRung for ImpactedTests {
    fn name(&self) -> &str {
        "impacted-tests"
    }
    fn level(&self) -> u8 {
        3
    }
    fn expected_ms(&self) -> u64 {
        self.expected_ms
    }

    fn check(&self, ctx: &VerifyCtx<'_>) -> RungOutcome {
        let Some(ws) = ctx.workspace else {
            return RungOutcome::unavailable("no workspace directory for the test rung");
        };
        if ctx.impact.tests.is_empty() {
            if ctx.delta.is_empty() {
                // Nothing changed, so nothing is reachable. That is a result, not a skip.
                return RungOutcome::pass();
            }

            // Something changed and selection found no tests for it. Do not believe that.
            //
            // Test selection here is a syntactic approximation, and measuring it on this
            // repository showed it under-selecting badly on exactly the entities that matter
            // most: a change to the core hashing function resolved to zero tests, because its
            // name is too short and too common to resolve across files. Passing on an empty
            // selection turns that into a green run, which is the one outcome that makes the
            // whole runtime untrustworthy. Running everything costs minutes; believing a false
            // green costs the premise.
            return match run_tool(&self.tool, ws, &[]) {
                Err(e) => {
                    RungOutcome::unavailable(format!("{} could not run: {e}", self.tool.program))
                }
                Ok(out) if out.status.success() => RungOutcome::pass(),
                Ok(out) => RungOutcome::fail(summarise_test_failure(&out, "the suite")),
            };
        }

        let selected: Vec<&String> = ctx.impact.tests.iter().collect();
        if selected.len() > self.fallback_to_all_above {
            return match run_tool(&self.tool, ws, &[]) {
                Err(e) => {
                    RungOutcome::unavailable(format!("{} could not run: {e}", self.tool.program))
                }
                Ok(out) if out.status.success() => RungOutcome::pass(),
                Ok(out) => RungOutcome::fail(summarise_test_failure(&out, "the suite")),
            };
        }

        for name in selected {
            match run_tool(&self.tool, ws, &[name.to_string()]) {
                Err(e) => {
                    return RungOutcome::unavailable(format!(
                        "{} could not run: {e}",
                        self.tool.program
                    ));
                }
                Ok(out) if out.status.success() => continue,
                // Stop at the first failure: the model needs one diagnostic to act on, not ten.
                Ok(out) => return RungOutcome::fail(summarise_test_failure(&out, name)),
            }
        }
        RungOutcome::pass()
    }
}

/// Rung 4: the whole suite, once, at the end of the task.
pub struct FullSuite {
    pub tool: ToolSpec,
    pub expected_ms: u64,
}

impl VerifyRung for FullSuite {
    fn name(&self) -> &str {
        "full-suite"
    }
    fn level(&self) -> u8 {
        4
    }
    fn expected_ms(&self) -> u64 {
        self.expected_ms
    }
    fn once_per_task(&self) -> bool {
        true
    }

    fn check(&self, ctx: &VerifyCtx<'_>) -> RungOutcome {
        let Some(ws) = ctx.workspace else {
            return RungOutcome::unavailable("no workspace directory for the full suite");
        };
        match run_tool(&self.tool, ws, &[]) {
            Err(e) => RungOutcome::unavailable(format!("{} could not run: {e}", self.tool.program)),
            Ok(out) if out.status.success() => RungOutcome::pass(),
            Ok(out) => RungOutcome::fail(summarise_test_failure(&out, "the suite")),
        }
    }
}

fn summarise_test_failure(out: &std::process::Output, what: &str) -> Vec<Diagnostic> {
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let mut diags = Vec::new();
    for line in stdout.lines().chain(stderr.lines()) {
        let t = line.trim();
        if t.starts_with("assertion")
            || t.contains("panicked at")
            || t.starts_with("test ") && t.ends_with("FAILED")
        {
            diags.push(Diagnostic::new("test-failed", t.to_string()));
        }
    }
    if diags.is_empty() {
        diags.push(Diagnostic::new(
            "test-failed",
            format!("{what} failed without a parseable assertion"),
        ));
    }
    flash_adapter::cap_diagnostics(diags)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::LangSpec;
    use flash_adapter::{Artifact, Delta, ImpactSet, Outline};

    fn ctx_for<'a>(
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

    fn rust() -> &'static LangSpec {
        LangSpec::for_path("x.rs").unwrap()
    }

    #[test]
    fn rung_zero_passes_clean_code_and_locates_breakage() {
        let good = Artifact::new("x.rs", b"fn a() {}\n".to_vec());
        let outline = symbols::outline(rust(), &good).unwrap();
        let (d, i) = (Delta::default(), ImpactSet::default());
        let rung = Reparse { spec: rust() };
        assert!(rung.check(&ctx_for(&good, &outline, &d, &i)).passed);

        let bad = Artifact::new("x.rs", b"fn a() {\n\nfn b( {\n".to_vec());
        let bad_outline = symbols::outline(rust(), &bad).unwrap();
        let outcome = rung.check(&ctx_for(&bad, &bad_outline, &d, &i));
        assert!(!outcome.passed);
        assert!(
            outcome.diagnostics.iter().any(|d| d.line.is_some()),
            "a syntax diagnostic should carry a line number"
        );
    }

    #[test]
    fn lint_catches_a_reference_to_something_the_edit_deleted() {
        // No hand-injected edges: this is the outline as it really is after the deletion, which is
        // the only state this rung ever sees in production.
        let art = Artifact::new("x.rs", b"fn caller() { helper(); }\n".to_vec());
        let outline = symbols::outline(rust(), &art).unwrap();
        let delta = Delta {
            removed: vec!["fn:helper".into()],
            ..Default::default()
        };
        let impact = ImpactSet::default();
        let outcome = Lint { spec: rust() }.check(&ctx_for(&art, &outline, &delta, &impact));
        assert!(!outcome.passed);
        assert_eq!(outcome.diagnostics[0].code, "dangling-reference");
    }

    #[test]
    fn a_dangling_reference_is_matched_on_whole_identifiers() {
        // Deleting `parse` must not accuse `parser` of referring to it.
        let art = Artifact::new("x.rs", b"fn caller() { parser(); }\n".to_vec());
        let outline = symbols::outline(rust(), &art).unwrap();
        let delta = Delta {
            removed: vec!["fn:parse".into()],
            ..Default::default()
        };
        let impact = ImpactSet::default();
        let outcome = Lint { spec: rust() }.check(&ctx_for(&art, &outline, &delta, &impact));
        assert!(outcome.passed, "{:?}", outcome.diagnostics);
        assert!(mentions_word("let x = parse();", "parse"));
        assert!(!mentions_word("let x = parser();", "parse"));
        assert!(!mentions_word("let reparse = 1;", "parse"));
    }

    #[test]
    fn lint_stays_quiet_about_re_exports_and_trait_imports() {
        // The two shapes that made the old unused-import heuristic unusable. A rung 1 diagnostic
        // costs two repair attempts, so silence here is the feature.
        let rung = Lint { spec: rust() };
        let (d, i) = (Delta::default(), ImpactSet::default());

        for source in [
            "pub use crate::thing::Thing;\nfn a() { let _ = 1; }\n",
            "use std::io::Write;\nfn a(f: &mut std::fs::File) { let _ = f.write_all(b\"x\"); }\n",
        ] {
            let art = Artifact::new("x.rs", source.as_bytes().to_vec());
            let outline = symbols::outline(rust(), &art).unwrap();
            let outcome = rung.check(&ctx_for(&art, &outline, &d, &i));
            assert!(
                outcome.passed,
                "rung 1 fired on clean code: {:?}",
                outcome.diagnostics
            );
        }
    }

    #[test]
    fn two_trait_impls_of_the_same_method_are_distinct_entities() {
        // `impl Debug for T` and `impl Display for T` both define `fmt`. If they share an id, an
        // op naming that id is ambiguous and rung 1 reports a duplicate definition that is not one.
        let art = Artifact::new(
            "x.rs",
            b"struct T;\nimpl std::fmt::Debug for T { fn fmt(&self) {} }\nimpl std::fmt::Display for T { fn fmt(&self) {} }\n"
                .to_vec(),
        );
        let outline = symbols::outline(rust(), &art).unwrap();
        let fmts: Vec<&str> = outline
            .ids()
            .into_iter()
            .filter(|id| id.ends_with("fmt"))
            .collect();
        assert_eq!(fmts.len(), 2, "{fmts:?}");
        assert_ne!(fmts[0], fmts[1], "both impls collapsed to one id: {fmts:?}");

        let (d, i) = (Delta::default(), ImpactSet::default());
        let outcome = Lint { spec: rust() }.check(&ctx_for(&art, &outline, &d, &i));
        assert!(
            outcome.passed,
            "distinct impls must not read as duplicates: {:?}",
            outcome.diagnostics
        );
    }

    #[test]
    fn an_external_rung_without_a_workspace_reports_unavailable_not_pass() {
        let art = Artifact::new("x.rs", b"fn a() {}\n".to_vec());
        let outline = symbols::outline(rust(), &art).unwrap();
        let (d, i) = (Delta::default(), ImpactSet::default());
        let rung = Typecheck {
            tool: ToolSpec::rust_typecheck(),
            expected_ms: 4000,
        };
        let outcome = rung.check(&ctx_for(&art, &outline, &d, &i));
        assert!(outcome.unavailable);
        assert!(!outcome.passed, "unavailable must never read as a pass");
    }

    #[test]
    fn a_missing_toolchain_is_unavailable_rather_than_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let art = Artifact::new("x.rs", b"fn a() {}\n".to_vec());
        let outline = symbols::outline(rust(), &art).unwrap();
        let (d, i) = (Delta::default(), ImpactSet::default());
        let rung = Typecheck {
            tool: ToolSpec::new("definitely-not-a-real-program-xyz", &[]),
            expected_ms: 10,
        };
        let ctx = VerifyCtx {
            artifact: &art,
            outline: &outline,
            delta: &d,
            impact: &i,
            workspace: Some(dir.path()),
        };
        let outcome = rung.check(&ctx);
        assert!(outcome.unavailable, "{outcome:?}");
    }

    #[test]
    fn cargo_json_becomes_structured_diagnostics() {
        let line = r#"{"reason":"compiler-message","message":{"level":"error","code":{"code":"E0308"},"message":"mismatched types","spans":[{"line_start":12}]}}"#;
        let diags = parse_cargo_json(line);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "E0308");
        assert_eq!(diags[0].line, Some(12));
    }

    #[test]
    fn cargo_warnings_are_not_reported_as_errors() {
        let line = r#"{"reason":"compiler-message","message":{"level":"warning","code":{"code":"unused_variables"},"message":"unused","spans":[]}}"#;
        assert!(parse_cargo_json(line).is_empty());
    }

    #[test]
    fn an_empty_delta_means_nothing_to_run() {
        let art = Artifact::new("x.rs", b"fn a() {}\n".to_vec());
        let outline = symbols::outline(rust(), &art).unwrap();
        let d = Delta::default();
        let i = ImpactSet::default();
        let dir = tempfile::tempdir().unwrap();
        let rung = ImpactedTests {
            tool: ToolSpec::rust_test(),
            expected_ms: 3000,
            fallback_to_all_above: 10,
        };
        let ctx = VerifyCtx {
            artifact: &art,
            outline: &outline,
            delta: &d,
            impact: &i,
            workspace: Some(dir.path()),
        };
        assert!(rung.check(&ctx).passed);
    }

    #[test]
    fn a_change_that_selected_no_tests_falls_back_to_the_suite() {
        // The safety property. Selection is an approximation that was measured under-selecting on
        // this very repository, so an empty selection against a real delta must not read as a
        // pass. There is no toolchain in this temp directory, so the rung reports unavailable,
        // which proves it tried to run something rather than passing for free.
        let art = Artifact::new("x.rs", b"fn a() {}\n".to_vec());
        let outline = symbols::outline(rust(), &art).unwrap();
        let d = Delta {
            changed: vec!["fn:a".into()],
            ..Default::default()
        };
        let i = ImpactSet::default();
        let dir = tempfile::tempdir().unwrap();
        let rung = ImpactedTests {
            tool: ToolSpec::new("definitely-not-a-real-program-xyz", &[]),
            expected_ms: 3000,
            fallback_to_all_above: 10,
        };
        let ctx = VerifyCtx {
            artifact: &art,
            outline: &outline,
            delta: &d,
            impact: &i,
            workspace: Some(dir.path()),
        };
        let outcome = rung.check(&ctx);
        assert!(
            !outcome.passed,
            "an empty selection against a real change must never pass for free"
        );
        assert!(outcome.unavailable, "{outcome:?}");
    }
}

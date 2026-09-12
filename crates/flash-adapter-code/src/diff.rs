//! The fallback path: apply a unified diff to one file.
//!
//! Section 4.1: "fallback: unified diff for one file if an op fails twice. rate is a primary
//! metric." So this exists to be *measured*, not to be relied on. Every time it runs, the op
//! schema failed to express something the model needed to say, and that is the signal that the
//! schema is wrong for that language - the kill line in phase 1 is exactly this rate.
//!
//! The applier is tolerant about hunk line numbers (models get them wrong) and strict about
//! context (that is what makes an edit verifiable). It searches for the hunk's context near the
//! stated position and fails loudly rather than applying a hunk in the wrong place.

use flash_adapter::{AdapterError, Result};

struct Hunk {
    /// 1-based start line in the original, as claimed by the header.
    old_start: usize,
    /// ' ' context, '-' removal, '+' addition.
    lines: Vec<(char, String)>,
}

pub fn apply_unified(src: &str, diff: &str) -> Result<String> {
    let hunks = parse_hunks(diff)?;
    if hunks.is_empty() {
        return Err(AdapterError::BadOp("diff contains no hunks".into()));
    }

    let mut out: Vec<String> = src.lines().map(|s| s.to_string()).collect();
    // Apply bottom up so earlier line numbers stay valid.
    let mut ordered: Vec<&Hunk> = hunks.iter().collect();
    ordered.sort_by_key(|h| std::cmp::Reverse(h.old_start));

    for hunk in ordered {
        let expected: Vec<&String> = hunk
            .lines
            .iter()
            .filter(|(tag, _)| *tag == ' ' || *tag == '-')
            .map(|(_, text)| text)
            .collect();

        let at = locate(&out, &expected, hunk.old_start.saturating_sub(1)).ok_or_else(|| {
            AdapterError::BadOp(format!(
                "diff hunk at line {} does not match the file: context not found",
                hunk.old_start
            ))
        })?;

        let mut replacement: Vec<String> = Vec::new();
        for (tag, text) in &hunk.lines {
            match tag {
                ' ' | '+' => replacement.push(text.clone()),
                '-' => {}
                _ => {}
            }
        }
        out.splice(at..at + expected.len(), replacement);
    }

    let mut text = out.join("\n");
    if src.ends_with('\n') && !text.ends_with('\n') {
        text.push('\n');
    }
    Ok(text)
}

/// Find where `expected` sits.
///
/// The claimed line number is a hint and nothing more: models miscount lines constantly, and a
/// diff that is right about content and wrong about position is still a correct edit. Context is
/// the anchor. When the context appears more than once, the occurrence nearest the claimed
/// position wins, which is the only tie-break that respects what the model was looking at.
fn locate(lines: &[String], expected: &[&String], hint: usize) -> Option<usize> {
    if expected.is_empty() {
        return Some(hint.min(lines.len()));
    }
    if expected.len() > lines.len() {
        return None;
    }
    let matches_at = |start: usize| -> bool {
        start + expected.len() <= lines.len()
            && expected
                .iter()
                .enumerate()
                .all(|(i, want)| lines[start + i].trim_end() == want.trim_end())
    };

    if matches_at(hint) {
        return Some(hint);
    }

    let last = lines.len() - expected.len();
    let mut best: Option<usize> = None;
    for start in 0..=last {
        if !matches_at(start) {
            continue;
        }
        best = match best {
            None => Some(start),
            Some(b) => Some(if start.abs_diff(hint) < b.abs_diff(hint) {
                start
            } else {
                b
            }),
        };
    }
    best
}

fn parse_hunks(diff: &str) -> Result<Vec<Hunk>> {
    let mut hunks: Vec<Hunk> = Vec::new();
    let mut current: Option<Hunk> = None;

    for line in diff.lines() {
        if line.starts_with("@@") {
            if let Some(h) = current.take() {
                hunks.push(h);
            }
            current = Some(Hunk {
                old_start: parse_old_start(line)?,
                lines: Vec::new(),
            });
            continue;
        }
        if line.starts_with("--- ") || line.starts_with("+++ ") || line.starts_with("diff ") {
            continue;
        }
        if let Some(h) = current.as_mut() {
            let mut chars = line.chars();
            match chars.next() {
                Some(tag @ (' ' | '+' | '-')) => h.lines.push((tag, chars.as_str().to_string())),
                // A truly empty line in a diff body is a context line that lost its space.
                None => h.lines.push((' ', String::new())),
                Some('\\') => {} // "\ No newline at end of file"
                Some(_) => {
                    return Err(AdapterError::BadOp(format!(
                        "unexpected line in diff body: {line}"
                    )));
                }
            }
        }
    }
    if let Some(h) = current {
        hunks.push(h);
    }
    Ok(hunks)
}

/// `@@ -12,7 +12,8 @@` -> 12
fn parse_old_start(header: &str) -> Result<usize> {
    let minus = header
        .split_whitespace()
        .find(|t| t.starts_with('-'))
        .ok_or_else(|| AdapterError::BadOp(format!("malformed hunk header: {header}")))?;
    let digits: String = minus[1..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits
        .parse()
        .map_err(|_| AdapterError::BadOp(format!("malformed hunk header: {header}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "fn one() {\n    1\n}\n\nfn two() {\n    2\n}\n";

    #[test]
    fn a_well_formed_diff_applies() {
        let diff = "--- a/x.rs\n+++ b/x.rs\n@@ -5,3 +5,3 @@\n fn two() {\n-    2\n+    22\n }\n";
        let out = apply_unified(SRC, diff).unwrap();
        assert!(out.contains("    22"), "{out}");
        assert!(out.contains("fn one()"), "{out}");
    }

    #[test]
    fn line_numbers_may_be_wrong_as_long_as_context_is_right() {
        // Models routinely miscount. Context is the real anchor.
        let diff = "@@ -99,3 +99,3 @@\n fn two() {\n-    2\n+    22\n }\n";
        let out = apply_unified(SRC, diff).unwrap();
        assert!(out.contains("    22"), "{out}");
    }

    #[test]
    fn context_that_does_not_exist_is_refused() {
        let diff = "@@ -1,3 +1,3 @@\n fn three() {\n-    3\n+    33\n }\n";
        let err = apply_unified(SRC, diff).unwrap_err();
        assert!(format!("{err}").contains("context not found"), "{err}");
    }

    #[test]
    fn several_hunks_apply_without_shifting_each_other() {
        let diff = "@@ -1,3 +1,3 @@\n fn one() {\n-    1\n+    11\n }\n@@ -5,3 +5,3 @@\n fn two() {\n-    2\n+    22\n }\n";
        let out = apply_unified(SRC, diff).unwrap();
        assert!(out.contains("    11"), "{out}");
        assert!(out.contains("    22"), "{out}");
    }

    #[test]
    fn an_empty_diff_is_an_error_not_a_silent_noop() {
        assert!(apply_unified(SRC, "").is_err());
    }

    #[test]
    fn trailing_newline_is_preserved() {
        let diff = "@@ -1,3 +1,3 @@\n fn one() {\n-    1\n+    11\n }\n";
        let out = apply_unified(SRC, diff).unwrap();
        assert!(out.ends_with('\n'));
    }
}

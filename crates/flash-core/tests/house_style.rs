//! House style, enforced rather than remembered.
//!
//! A style rule that lives in someone's head is a rule that comes back the first time anyone is
//! in a hurry. This one is cheap to check and the check is the only thing that makes it real.

use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .to_path_buf()
}

fn text_files() -> Vec<PathBuf> {
    let root = workspace_root();
    let mut out = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if path.is_dir() {
                if matches!(name.as_ref(), "target" | ".git" | ".flash") {
                    continue;
                }
                stack.push(path);
            } else if matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("rs") | Some("md") | Some("toml") | Some("json")
            ) {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

#[test]
fn no_em_or_en_dashes_anywhere() {
    let root = workspace_root();
    let mut offenders = Vec::new();

    for path in text_files() {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for (n, line) in text.lines().enumerate() {
            if line.contains('\u{2014}') || line.contains('\u{2013}') {
                let rel = path.strip_prefix(&root).unwrap_or(&path);
                offenders.push(format!(
                    "{}:{}: {}",
                    rel.display(),
                    n + 1,
                    line.trim().chars().take(90).collect::<String>()
                ));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "em and en dashes are not house style; use a comma, a colon, a full stop, or the word \
         \"to\". Found {}:\n{}",
        offenders.len(),
        offenders.join("\n")
    );
}

#[test]
fn no_trailing_whitespace() {
    let root = workspace_root();
    let mut offenders = Vec::new();
    for path in text_files() {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for (n, line) in text.lines().enumerate() {
            if line.len() != line.trim_end().len() {
                let rel = path.strip_prefix(&root).unwrap_or(&path);
                offenders.push(format!("{}:{}", rel.display(), n + 1));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "trailing whitespace in {} places:\n{}",
        offenders.len(),
        offenders.join("\n")
    );
}

//! Materialize a working tree out of the content store, without copying the bytes.
//!
//! The problem this solves is one people hit before they hit anything flash was built for: run
//! four agents on one repository and you have four checkouts, four target directories, and a disk
//! that fills up. Each checkout is mostly the same bytes as the others.
//!
//! The store already holds every file version exactly once, keyed by its blake3 hash, because
//! that is what content addressing means. So a working tree is a directory of hard links into
//! that store. Ten trees of the same revision cost one copy of the bytes plus ten directory
//! entries. Switching a tree to another revision relinks only the files whose content differs.
//!
//! ## The sharp edge, stated plainly
//!
//! A hard link is not copy on write. Two names point at one inode, so a tool that opens a file
//! and writes into it *in place* writes into the store as well, and into every other tree sharing
//! that content.
//!
//! In practice almost nothing does this: editors, compilers, formatters and git itself write a
//! temporary file and rename it over the target, which breaks the link and leaves everything else
//! untouched. This is the same bet pnpm makes for node_modules, and it holds up at scale. What
//! makes it safe rather than lucky is that store blobs are marked read only, so an in place write
//! fails loudly instead of corrupting shared content silently.
//!
//! [`Worktree::detach`] is the escape hatch for a tool that genuinely needs to mutate in place:
//! it replaces a link with a private copy.

use crate::{ContentStore, Result, StoreError, ensure_dir};
use flash_core::Digest;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// What a tree contains: workspace-relative path to content hash.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TreeSpec {
    pub files: Vec<(String, Digest)>,
}

impl TreeSpec {
    pub fn add(&mut self, path: impl Into<String>, content: Digest) {
        self.files.push((path.into(), content));
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

/// What materializing cost, so the saving is a number rather than a claim.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaterializeStats {
    pub files: usize,
    /// Bytes the tree appears to contain.
    pub logical_bytes: u64,
    /// Files that were hard linked to content already in the store.
    pub linked: usize,
    /// Files that had to be copied because the filesystem refused a link.
    pub copied: usize,
    /// Files already present with the right content, left alone.
    pub unchanged: usize,
    /// Files removed because the new tree does not contain them.
    pub removed: usize,
}

impl MaterializeStats {
    /// Bytes actually written to this tree. A fully shared tree writes none.
    pub fn bytes_written(&self) -> u64 {
        if self.files == 0 {
            return 0;
        }
        let per_file = self.logical_bytes / self.files as u64;
        per_file * self.copied as u64
    }
}

pub struct Worktree<'a> {
    store: &'a ContentStore,
    root: PathBuf,
}

impl<'a> Worktree<'a> {
    pub fn new(store: &'a ContentStore, root: impl Into<PathBuf>) -> Self {
        Worktree {
            store,
            root: root.into(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Lay down exactly `spec`, relinking what differs and deleting what is no longer in it.
    ///
    /// Switching an existing tree from one revision to another therefore touches only the files
    /// that actually differ, which is the same principle as everything else here: the work is
    /// proportional to the change, not to the size of the thing being changed.
    pub fn materialize(&self, spec: &TreeSpec) -> Result<MaterializeStats> {
        ensure_dir(&self.root)?;
        let mut stats = MaterializeStats::default();

        let wanted: std::collections::BTreeMap<&str, Digest> =
            spec.files.iter().map(|(p, d)| (p.as_str(), *d)).collect();

        for (rel, digest) in &wanted {
            let dest = self.root.join(rel);
            if let Some(parent) = dest.parent() {
                ensure_dir(parent)?;
            }

            let bytes = self.store.get(digest)?;
            stats.files += 1;
            stats.logical_bytes += bytes.len() as u64;

            // Already correct? Leave it alone; relinking an unchanged file would throw away a
            // tool's mtime for nothing.
            if dest.exists()
                && let Ok(existing) = std::fs::read(&dest)
                && Digest::of(&existing) == *digest
            {
                stats.unchanged += 1;
                continue;
            }

            if dest.exists() {
                let _ = make_writable(&dest);
                let _ = std::fs::remove_file(&dest);
            }

            let source = self.store.path_of(digest);
            match std::fs::hard_link(&source, &dest) {
                Ok(()) => stats.linked += 1,
                Err(_) => {
                    // Different volume, a filesystem without links, or a hit link limit. A copy
                    // is correct and only costs disk.
                    std::fs::write(&dest, &bytes).map_err(|source| StoreError::Io {
                        path: dest.clone(),
                        source,
                    })?;
                    stats.copied += 1;
                }
            }
        }

        stats.removed = self.remove_extraneous(&wanted)?;
        Ok(stats)
    }

    /// Replace one linked file with a private copy, for a tool that must write in place.
    pub fn detach(&self, rel: &str) -> Result<()> {
        let dest = self.root.join(rel);
        let bytes = std::fs::read(&dest).map_err(|source| StoreError::Io {
            path: dest.clone(),
            source,
        })?;
        let tmp = dest.with_extension("flash-detach");
        std::fs::write(&tmp, &bytes).map_err(|source| StoreError::Io {
            path: tmp.clone(),
            source,
        })?;
        make_writable(&tmp).ok();
        std::fs::rename(&tmp, &dest).map_err(|source| StoreError::Io { path: dest, source })
    }

    /// Take whatever is on disk into the store, and describe it as a tree.
    ///
    /// This is how an existing checkout becomes shareable: ingest once, and every later tree of
    /// the same content is free.
    pub fn ingest(&self, skip: &[&str]) -> Result<TreeSpec> {
        let mut spec = TreeSpec::default();
        let mut stack = vec![self.root.clone()];
        while let Some(dir) = stack.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name();
                let name = name.to_string_lossy().to_string();
                if skip.iter().any(|s| *s == name) {
                    continue;
                }
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let bytes = match std::fs::read(&path) {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                let digest = self.store.put(&bytes)?;
                let rel = path
                    .strip_prefix(&self.root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                spec.add(rel, digest);
            }
        }
        spec.files.sort();
        Ok(spec)
    }

    fn remove_extraneous(
        &self,
        wanted: &std::collections::BTreeMap<&str, Digest>,
    ) -> Result<usize> {
        let mut removed = 0;
        let mut stack = vec![self.root.clone()];
        while let Some(dir) = stack.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let rel = path
                    .strip_prefix(&self.root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                if !wanted.contains_key(rel.as_str()) {
                    let _ = make_writable(&path);
                    if std::fs::remove_file(&path).is_ok() {
                        removed += 1;
                    }
                }
            }
        }
        Ok(removed)
    }
}

fn make_writable(path: &Path) -> std::io::Result<()> {
    let mut perms = std::fs::metadata(path)?.permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(path, perms)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(dir: &Path) -> ContentStore {
        ContentStore::open(dir.join("blobs")).unwrap()
    }

    fn spec_of(store: &ContentStore, files: &[(&str, &str)]) -> TreeSpec {
        let mut spec = TreeSpec::default();
        for (path, body) in files {
            spec.add(*path, store.put(body.as_bytes()).unwrap());
        }
        spec
    }

    #[test]
    fn a_tree_materializes_with_the_right_content() {
        let dir = tempfile::tempdir().unwrap();
        let cs = store(dir.path());
        let spec = spec_of(&cs, &[("src/lib.rs", "fn a() {}\n"), ("README.md", "hi\n")]);

        let wt = Worktree::new(&cs, dir.path().join("tree"));
        let stats = wt.materialize(&spec).unwrap();

        assert_eq!(stats.files, 2);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("tree/src/lib.rs")).unwrap(),
            "fn a() {}\n"
        );
        assert_eq!(stats.linked + stats.copied, 2);
    }

    #[test]
    fn two_trees_of_the_same_content_share_the_bytes() {
        // The whole point. Two trees, one copy on disk.
        let dir = tempfile::tempdir().unwrap();
        let cs = store(dir.path());
        let spec = spec_of(&cs, &[("a.rs", "fn a() {}\n"), ("b.rs", "fn b() {}\n")]);

        let one = Worktree::new(&cs, dir.path().join("agent-1"));
        let two = Worktree::new(&cs, dir.path().join("agent-2"));
        let s1 = one.materialize(&spec).unwrap();
        let s2 = two.materialize(&spec).unwrap();

        assert_eq!(s1.files, 2);
        assert_eq!(s2.files, 2);
        // On a filesystem with links, the second tree writes nothing at all.
        if s2.linked == 2 {
            assert_eq!(s2.bytes_written(), 0, "a shared tree should write no bytes");
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join("agent-2/a.rs")).unwrap(),
            "fn a() {}\n"
        );
    }

    #[test]
    fn switching_revisions_touches_only_what_differs() {
        let dir = tempfile::tempdir().unwrap();
        let cs = store(dir.path());
        let old = spec_of(&cs, &[("a.rs", "fn a() {}\n"), ("b.rs", "fn b() {}\n")]);
        let new = spec_of(&cs, &[("a.rs", "fn a() {}\n"), ("b.rs", "fn b() { 1 }\n")]);

        let wt = Worktree::new(&cs, dir.path().join("tree"));
        wt.materialize(&old).unwrap();
        let stats = wt.materialize(&new).unwrap();

        assert_eq!(
            stats.unchanged, 1,
            "a.rs did not move and should be left alone"
        );
        assert_eq!(stats.linked + stats.copied, 1, "only b.rs is rewritten");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("tree/b.rs")).unwrap(),
            "fn b() { 1 }\n"
        );
    }

    #[test]
    fn a_file_that_left_the_tree_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let cs = store(dir.path());
        let before = spec_of(&cs, &[("keep.rs", "1\n"), ("gone.rs", "2\n")]);
        let after = spec_of(&cs, &[("keep.rs", "1\n")]);

        let wt = Worktree::new(&cs, dir.path().join("tree"));
        wt.materialize(&before).unwrap();
        let stats = wt.materialize(&after).unwrap();

        assert_eq!(stats.removed, 1);
        assert!(!dir.path().join("tree/gone.rs").exists());
    }

    #[test]
    fn ingest_then_materialize_round_trips_a_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let cs = store(dir.path());
        let src = dir.path().join("checkout");
        std::fs::create_dir_all(src.join("src")).unwrap();
        std::fs::write(src.join("src/lib.rs"), "fn a() {}\n").unwrap();
        std::fs::write(src.join("Cargo.toml"), "[package]\n").unwrap();

        let origin = Worktree::new(&cs, &src);
        let spec = origin.ingest(&[".git", "target"]).unwrap();
        assert_eq!(spec.len(), 2);

        let clone = Worktree::new(&cs, dir.path().join("clone"));
        clone.materialize(&spec).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("clone/src/lib.rs")).unwrap(),
            "fn a() {}\n"
        );
    }

    #[test]
    fn ingest_skips_what_it_is_told_to_skip() {
        let dir = tempfile::tempdir().unwrap();
        let cs = store(dir.path());
        let src = dir.path().join("checkout");
        std::fs::create_dir_all(src.join("target")).unwrap();
        std::fs::write(src.join("target/huge.bin"), vec![0u8; 1024]).unwrap();
        std::fs::write(src.join("keep.rs"), "1\n").unwrap();

        let spec = Worktree::new(&cs, &src).ingest(&["target"]).unwrap();
        assert_eq!(spec.len(), 1);
        assert_eq!(spec.files[0].0, "keep.rs");
    }

    #[test]
    fn detach_gives_a_tree_its_own_copy() {
        let dir = tempfile::tempdir().unwrap();
        let cs = store(dir.path());
        let spec = spec_of(&cs, &[("a.rs", "shared\n")]);

        let one = Worktree::new(&cs, dir.path().join("one"));
        let two = Worktree::new(&cs, dir.path().join("two"));
        one.materialize(&spec).unwrap();
        two.materialize(&spec).unwrap();

        one.detach("a.rs").unwrap();
        std::fs::write(dir.path().join("one/a.rs"), "changed\n").unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.path().join("two/a.rs")).unwrap(),
            "shared\n",
            "writing to a detached file must not reach the other tree"
        );
    }
}

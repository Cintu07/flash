//! A repository-wide symbol index, so impact analysis can cross file boundaries.
//!
//! Per-file impact answers "which tests *in this file* can reach the change". That is the easy
//! half and it is not the half that matters. In a real project the changed function lives in one
//! file, its caller in another, and the test in a third, so a single-file analysis reports no
//! impacted tests and the ladder's rung 3 passes trivially. That is exactly the failure this
//! runtime already hit once, and finding it in production was luck.
//!
//! The index resolves references across every indexed file and answers reachability over the
//! whole graph. It is built from outlines the adapter already produces, and it is content
//! addressed by the set of file hashes that went into it, so it caches like everything else: add
//! one file, rebuild; change nothing, reuse.
//!
//! ## Measured limits, because tuning did not fix them
//!
//! Resolution is by simple name, and on this repository that was measured doing both wrong things
//! at once.
//!
//! Linking every matching name produced one hairball: `new`, `of`, `parse` and `default` are each
//! defined in a dozen files, so every entity reached all 46 files and all 246 tests. Filtering
//! ambiguous and short names fixed the leaves, and a private helper now correctly reaches one
//! test, but it broke the important case in the other direction: `fn:Digest::of` is called
//! everywhere and now resolves to zero tests, because `of` is too short and too common to carry
//! information. Meanwhile `fn:percentile` still reaches 229 tests, because any path that touches
//! a widely used type climbs into everything above it.
//!
//! So this is a stopgap with a known shape, not name resolution:
//!
//! * it discriminates well on entities with distinctive names in few files;
//! * it under-selects on short or common names, which is why `ImpactedTests` runs the whole suite
//!   rather than passing when selection comes back empty against a real change;
//! * it over-selects through widely used types, which costs test time and nothing else.
//!
//! The real fixes are name resolution from an lsp, or coverage data saying which tests actually
//! executed which lines. Both keep this interface. Neither is threshold tuning, which is what the
//! numbers above rule out.

use crate::symbols;
use flash_adapter::{Artifact, Delta, ImpactSet, Outline};
use flash_core::{Digest, Hasher};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntityRef {
    pub path: String,
    pub kind: String,
    /// Test runner selector, for entities that are tests.
    pub selector: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RepoIndex {
    /// Fully qualified entity id to where it lives.
    entities: BTreeMap<String, EntityRef>,
    /// Who refers to whom, resolved across files.
    referrers: BTreeMap<String, BTreeSet<String>>,
    /// The content that produced this index, so a stale index is detectable rather than trusted.
    inputs: Vec<(String, Digest)>,
}

impl RepoIndex {
    /// Build from parsed files. `files` is (artifact, outline) for everything in scope.
    pub fn build(files: &[(Artifact, Outline)]) -> RepoIndex {
        let mut index = RepoIndex::default();

        // Pass one: every definition, and a name table to resolve against.
        let mut by_name: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (artifact, outline) in files {
            index
                .inputs
                .push((artifact.path.clone(), artifact.digest()));
            for e in &outline.entities {
                if e.kind == "import" {
                    continue;
                }
                let selector = (e.kind == "test").then(|| {
                    e.id.split_once(':')
                        .map(|(_, r)| r.to_string())
                        .unwrap_or_else(|| e.id.clone())
                });
                index.entities.insert(
                    e.id.clone(),
                    EntityRef {
                        path: artifact.path.clone(),
                        kind: e.kind.clone(),
                        selector,
                    },
                );
                by_name
                    .entry(simple_name(&e.id))
                    .or_default()
                    .push(e.id.clone());
            }
        }
        index.inputs.sort();

        // A name defined in many places carries no information about who calls whom.
        //
        // The first version of this linked on every matching simple name, and the result was one
        // hairball: `new`, `of`, `parse`, `default` and `check` are defined in a dozen files each,
        // so everything linked to everything and every entity reached all 246 tests in this
        // repository. A cross file index that selects the whole suite is worse than no index,
        // because it looks like it is working.
        //
        // So an ambiguous name is dropped rather than linked to all of its definitions. A short
        // name goes the same way: three characters is not enough to identify anything across a
        // workspace. Both of these lose real edges, which is the honest cost, and the cost lands
        // on `reachable_tests` under-selecting in exactly the modules that reuse names. That is
        // why this is documented as an approximation rather than sold as resolution.
        const MAX_DEFINITIONS: usize = 3;
        const MIN_NAME_LEN: usize = 4;
        let ambiguous: BTreeSet<&String> = by_name
            .iter()
            .filter(|(name, ids)| ids.len() > MAX_DEFINITIONS || name.len() < MIN_NAME_LEN)
            .map(|(name, _)| name)
            .collect();

        // Pass two: resolve every identifier inside every body against the whole repo, not just
        // the file it appears in. This is the only difference from per-file analysis, and it is
        // the difference between rung 3 selecting tests and rung 3 selecting nothing.
        for (artifact, outline) in files {
            let text = artifact.text().to_string();
            for e in &outline.entities {
                if e.kind == "import" {
                    continue;
                }
                let (bs, be) = e.body.unwrap_or((e.start, e.end));
                let body = text
                    .get(bs.min(text.len())..be.min(text.len()))
                    .unwrap_or("");
                let mut linked = BTreeSet::new();
                for word in symbols::identifiers(body) {
                    if ambiguous.contains(&word.to_string()) {
                        continue;
                    }
                    if let Some(ids) = by_name.get(word) {
                        for id in ids {
                            if *id != e.id {
                                linked.insert(id.clone());
                            }
                        }
                    }
                }
                for target in linked {
                    index
                        .referrers
                        .entry(target)
                        .or_default()
                        .insert(e.id.clone());
                }
            }
        }

        index
    }

    pub fn len(&self) -> usize {
        self.entities.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }

    pub fn files(&self) -> usize {
        self.inputs.len()
    }

    pub fn get(&self, id: &str) -> Option<&EntityRef> {
        self.entities.get(id)
    }

    /// Identity of the inputs. Two indexes with the same digest describe the same repository
    /// state, which is what lets an index be cached in the content store like any other node.
    pub fn digest(&self) -> Digest {
        let mut h = Hasher::new("flash.repo-index.v1");
        for (path, content) in &self.inputs {
            h.str(path);
            h.digest(content);
        }
        h.finish()
    }

    /// Is this index still describing the files on disk?
    pub fn is_current(&self, files: &[(String, Digest)]) -> bool {
        let mut sorted = files.to_vec();
        sorted.sort();
        sorted == self.inputs
    }

    /// Everything that transitively refers to `seeds`, across files.
    pub fn reachable(&self, seeds: &[String]) -> BTreeSet<String> {
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut queue: VecDeque<String> = seeds.iter().cloned().collect();
        while let Some(id) = queue.pop_front() {
            if !seen.insert(id.clone()) {
                continue;
            }
            if let Some(ups) = self.referrers.get(&id) {
                for up in ups {
                    if !seen.contains(up) {
                        queue.push_back(up.clone());
                    }
                }
            }
        }
        seen
    }

    /// Test selectors for every test that can reach the change, wherever it lives.
    pub fn reachable_tests(&self, seeds: &[String]) -> Vec<String> {
        let mut out: Vec<String> = self
            .reachable(seeds)
            .into_iter()
            .filter_map(|id| self.entities.get(&id).and_then(|e| e.selector.clone()))
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// Which files hold anything the change can reach. What rung 2 should re-typecheck.
    pub fn reachable_files(&self, seeds: &[String]) -> Vec<String> {
        let mut out: Vec<String> = self
            .reachable(seeds)
            .into_iter()
            .filter_map(|id| self.entities.get(&id).map(|e| e.path.clone()))
            .collect();
        out.sort();
        out.dedup();
        out
    }

    pub fn impact(&self, delta: &Delta) -> ImpactSet {
        let seeds: Vec<String> = delta.touched().into_iter().map(str::to_string).collect();
        if seeds.is_empty() {
            return ImpactSet::default();
        }
        let reached = self.reachable(&seeds);
        ImpactSet {
            tests: self.reachable_tests(&seeds),
            units: self
                .reachable_files(&seeds)
                .into_iter()
                .map(|p| format!("file:{p}"))
                .collect(),
            entities: reached
                .into_iter()
                // A removed entity no longer exists to be checked, and offering it up as work is
                // how a repair loop ends up chasing something that is not there.
                .filter(|id| self.entities.contains_key(id) || !delta.removed.contains(id))
                .collect(),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("a repo index is serializable")
    }

    pub fn decode(bytes: &[u8]) -> Option<RepoIndex> {
        serde_json::from_slice(bytes).ok()
    }
}

fn simple_name(id: &str) -> String {
    let qualified = id.split_once(':').map(|(_, r)| r).unwrap_or(id);
    qualified
        .rsplit([':', '.'])
        .next()
        .unwrap_or(qualified)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CodeAdapter;
    use flash_adapter::Adapter;

    /// Three files, one call chain, and the test three hops from the change.
    fn three_files() -> Vec<(Artifact, Outline)> {
        let adapter = CodeAdapter::new();
        let files = [
            ("src/core.rs", "pub fn core() -> u32 {\n    1\n}\n"),
            (
                "src/mid.rs",
                "use crate::core::core;\n\npub fn wrapper() -> u32 {\n    core() + 1\n}\n",
            ),
            (
                "src/tests.rs",
                "use crate::mid::wrapper;\n\n#[test]\nfn checks_wrapper() {\n    assert_eq!(wrapper(), 2);\n}\n\n#[test]\nfn checks_nothing() {\n    assert!(true);\n}\n",
            ),
        ];
        files
            .iter()
            .map(|(p, body)| {
                let a = Artifact::new(*p, body.as_bytes().to_vec());
                let o = adapter.outline(&a).unwrap();
                (a, o)
            })
            .collect()
    }

    #[test]
    fn a_change_reaches_a_test_three_files_away() {
        let index = RepoIndex::build(&three_files());
        let tests = index.reachable_tests(&["fn:core".to_string()]);
        assert!(
            tests.contains(&"checks_wrapper".to_string()),
            "the test two hops and two files away must be selected: {tests:?}"
        );
    }

    #[test]
    fn per_file_analysis_cannot_find_it_which_is_the_whole_point() {
        // The same change, through the single-file path, selects nothing. This test exists so the
        // difference is a fact in the suite rather than a claim in a readme.
        let adapter = CodeAdapter::new();
        let files = three_files();
        let (core_artifact, core_outline) = &files[0];
        let _ = core_artifact;
        let delta = Delta {
            changed: vec!["fn:core".into()],
            ..Default::default()
        };
        let per_file = adapter.impact(core_outline, &delta);
        assert!(
            per_file.tests.is_empty(),
            "single file impact should find nothing here: {:?}",
            per_file.tests
        );

        let index = RepoIndex::build(&files);
        assert!(!index.impact(&delta).tests.is_empty());
    }

    #[test]
    fn an_unrelated_test_is_not_dragged_in() {
        let index = RepoIndex::build(&three_files());
        let tests = index.reachable_tests(&["fn:core".to_string()]);
        assert!(!tests.contains(&"checks_nothing".to_string()), "{tests:?}");
    }

    #[test]
    fn impact_names_the_files_worth_rechecking() {
        let index = RepoIndex::build(&three_files());
        let delta = Delta {
            changed: vec!["fn:core".into()],
            ..Default::default()
        };
        let units = index.impact(&delta).units;
        assert!(units.iter().any(|u| u.contains("mid.rs")), "{units:?}");
        assert!(units.iter().any(|u| u.contains("tests.rs")), "{units:?}");
    }

    #[test]
    fn the_index_is_identified_by_its_inputs() {
        let a = RepoIndex::build(&three_files());
        let b = RepoIndex::build(&three_files());
        assert_eq!(a.digest(), b.digest(), "same files, same index identity");

        let adapter = CodeAdapter::new();
        let mut files = three_files();
        let changed = Artifact::new(
            "src/core.rs",
            b"pub fn core() -> u32 {\n    2\n}\n".to_vec(),
        );
        let outline = adapter.outline(&changed).unwrap();
        files[0] = (changed, outline);
        assert_ne!(
            a.digest(),
            RepoIndex::build(&files).digest(),
            "changed content must change the index identity"
        );
    }

    #[test]
    fn a_stale_index_reports_itself_as_stale() {
        let files = three_files();
        let index = RepoIndex::build(&files);
        let current: Vec<(String, Digest)> = files
            .iter()
            .map(|(a, _)| (a.path.clone(), a.digest()))
            .collect();
        assert!(index.is_current(&current));

        let mut moved = current.clone();
        moved[0].1 = Digest::of(b"something else");
        assert!(!index.is_current(&moved));
    }

    #[test]
    fn the_index_round_trips_so_it_can_be_cached() {
        let index = RepoIndex::build(&three_files());
        let back = RepoIndex::decode(&index.encode()).expect("decodes");
        assert_eq!(back.digest(), index.digest());
        assert_eq!(
            back.reachable_tests(&["fn:core".to_string()]),
            index.reachable_tests(&["fn:core".to_string()])
        );
    }

    #[test]
    fn a_cycle_between_files_terminates() {
        let adapter = CodeAdapter::new();
        let files: Vec<(Artifact, Outline)> = [
            ("a.rs", "pub fn ping() {\n    pong();\n}\n"),
            ("b.rs", "pub fn pong() {\n    ping();\n}\n"),
        ]
        .iter()
        .map(|(p, b)| {
            let a = Artifact::new(*p, b.as_bytes().to_vec());
            let o = adapter.outline(&a).unwrap();
            (a, o)
        })
        .collect();

        let index = RepoIndex::build(&files);
        let reached = index.reachable(&["fn:ping".to_string()]);
        assert!(reached.contains("fn:pong"));
    }

    #[test]
    fn an_empty_delta_reaches_nothing() {
        let index = RepoIndex::build(&three_files());
        assert!(index.impact(&Delta::default()).tests.is_empty());
    }
}

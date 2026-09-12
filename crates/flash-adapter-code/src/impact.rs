//! Impact analysis: given what changed, what actually has to be re-checked (d7).
//!
//! This is the half of the system that makes warm cheap. The memo store stops work being
//! *repeated*; impact analysis stops work being *done in the first place*. Without it, every edit
//! drags the whole test suite behind it and the ladder's cheap rungs buy nothing.
//!
//! The traversal is over reverse reference edges: if a test calls a helper that calls the changed
//! function, the test is impacted. It is deliberately conservative - an edge that might exist is
//! kept - because the cost of one extra test is seconds and the cost of a missed one is a false
//! green, which is the only outcome that makes the whole runtime untrustworthy.

use flash_adapter::{Delta, ImpactSet, Outline};
use std::collections::{BTreeSet, HashMap, VecDeque};

/// Everything that transitively refers to the changed entities, plus the tests among them.
pub fn analyse(outline: &Outline, delta: &Delta) -> ImpactSet {
    if delta.is_empty() {
        return ImpactSet::default();
    }

    // Reverse edges: who refers to me.
    let mut referrers: HashMap<&str, Vec<&str>> = HashMap::new();
    for e in &outline.entities {
        for r in &e.refs {
            referrers.entry(r.as_str()).or_default().push(e.id.as_str());
        }
    }

    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut queue: VecDeque<String> = delta.touched().into_iter().map(str::to_string).collect();

    while let Some(id) = queue.pop_front() {
        if !seen.insert(id.clone()) {
            continue;
        }
        if let Some(ups) = referrers.get(id.as_str()) {
            for up in ups {
                if !seen.contains(*up) {
                    queue.push_back((*up).to_string());
                }
            }
        }
    }

    // A removed entity is in the impact set as a cause, but it no longer exists to be checked.
    for removed in &delta.removed {
        if outline.get(removed).is_none() {
            seen.remove(removed);
        }
    }

    let tests = seen
        .iter()
        .filter_map(|id| outline.get(id))
        .filter(|e| e.kind == "test")
        .map(|e| {
            // Test selectors are what a runner takes on the command line: the qualified name
            // without the kind prefix.
            e.id.split_once(':')
                .map(|(_, r)| r.to_string())
                .unwrap_or_else(|| e.id.clone())
        })
        .collect::<Vec<_>>();

    ImpactSet {
        entities: seen.into_iter().collect(),
        tests,
        units: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{lang::LangSpec, symbols};
    use flash_adapter::Artifact;

    fn outline_of(code: &str) -> Outline {
        let spec = LangSpec::for_path("x.rs").unwrap();
        symbols::outline(spec, &Artifact::new("x.rs", code.as_bytes().to_vec())).unwrap()
    }

    const CODE: &str = r#"
fn core() -> u32 { 1 }

fn wrapper() -> u32 { core() + 1 }

fn unrelated() -> u32 { 99 }

#[test]
fn tests_wrapper() { assert_eq!(wrapper(), 2); }

#[test]
fn tests_unrelated() { assert_eq!(unrelated(), 99); }
"#;

    #[test]
    fn impact_reaches_tests_through_a_call_chain() {
        let o = outline_of(CODE);
        let delta = Delta {
            changed: vec!["fn:core".into()],
            ..Default::default()
        };
        let impact = analyse(&o, &delta);
        assert!(
            impact.tests.iter().any(|t| t == "tests_wrapper"),
            "the test two hops away must be selected: {:?}",
            impact.tests
        );
    }

    #[test]
    fn impact_leaves_unrelated_tests_alone() {
        let o = outline_of(CODE);
        let delta = Delta {
            changed: vec!["fn:core".into()],
            ..Default::default()
        };
        let impact = analyse(&o, &delta);
        assert!(
            !impact.tests.iter().any(|t| t == "tests_unrelated"),
            "an unrelated test must not be dragged in: {:?}",
            impact.tests
        );
    }

    #[test]
    fn an_empty_delta_impacts_nothing() {
        let o = outline_of(CODE);
        assert!(analyse(&o, &Delta::default()).is_empty());
    }

    #[test]
    fn a_cycle_in_the_reference_graph_terminates() {
        let o = outline_of("fn a() { b(); }\nfn b() { a(); }\n");
        let delta = Delta {
            changed: vec!["fn:a".into()],
            ..Default::default()
        };
        let impact = analyse(&o, &delta);
        assert!(impact.entities.contains(&"fn:b".to_string()));
    }

    #[test]
    fn a_removed_entity_is_not_offered_up_for_checking() {
        let o = outline_of("fn stays() {}\n");
        let delta = Delta {
            removed: vec!["fn:gone".into()],
            ..Default::default()
        };
        let impact = analyse(&o, &delta);
        assert!(
            !impact.entities.contains(&"fn:gone".to_string()),
            "an entity that no longer exists cannot be re-verified"
        );
    }
}

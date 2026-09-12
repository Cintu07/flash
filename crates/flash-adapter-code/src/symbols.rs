//! Entity extraction: bytes in, named entities with stable ids out.
//!
//! Ids look like `fn:Parser::parse_header`, `struct:Header`, `test:tests::parses_a_header`.
//! They are built from names and enclosing scopes, never from offsets or ordinals, because every
//! cached thing downstream is keyed on them: move a function within a file and its id must not
//! change, or the whole file's worth of memo entries is thrown away for a formatting edit.
//!
//! Reference edges are a syntactic approximation: an identifier token inside an entity's body
//! that matches another entity's simple name counts as a reference. This is deliberately a
//! heuristic. It is enough for test selection (d7) and it costs microseconds; the PRD's phase 4
//! replaces it with coverage data, and an lsp can refine it before that. What matters is that it
//! never silently *misses* an edge in the common case, so impact analysis stays conservative.

use crate::lang::LangSpec;
use flash_adapter::{AdapterError, Artifact, Entity, Outline, Result};
use std::collections::{BTreeSet, HashMap};
use tree_sitter::{Node, Parser, Tree};

/// Parse an artifact, or say why it could not be parsed.
pub fn parse(spec: &LangSpec, artifact: &Artifact) -> Result<Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(&spec.language())
        .map_err(|e| AdapterError::Parse(format!("grammar {} unavailable: {e}", spec.name)))?;
    parser
        .parse(&artifact.bytes, None)
        .ok_or_else(|| AdapterError::Parse("parser returned no tree".into()))
}

/// Byte ranges of every ERROR or MISSING node. Rung 0 is exactly this being empty.
pub fn syntax_errors(tree: &Tree) -> Vec<(usize, usize, String)> {
    let mut out = Vec::new();
    let mut cursor = tree.walk();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.is_error() {
            out.push((
                node.start_byte(),
                node.end_byte(),
                "syntax error".to_string(),
            ));
            continue;
        }
        if node.is_missing() {
            out.push((
                node.start_byte(),
                node.end_byte(),
                format!("missing {}", node.kind()),
            ));
            continue;
        }
        if node.has_error() {
            for child in node.children(&mut cursor) {
                stack.push(child);
            }
        }
    }
    out.sort();
    out
}

pub fn outline(spec: &LangSpec, artifact: &Artifact) -> Result<Outline> {
    let tree = parse(spec, artifact)?;
    let src = artifact.bytes.as_slice();
    let mut entities: Vec<Entity> = Vec::new();

    collect(spec, tree.root_node(), src, &mut Vec::new(), &mut entities);
    collect_imports(spec, &tree, src, &mut entities);
    resolve_refs(spec, src, &mut entities);

    // A stable, readable order: definition order in the file.
    entities.sort_by_key(|e| (e.start, e.id.clone()));
    Ok(Outline {
        path: artifact.path.clone(),
        entities,
    })
}

fn node_text<'a>(node: Node<'_>, src: &'a [u8]) -> &'a str {
    std::str::from_utf8(&src[node.start_byte()..node.end_byte()]).unwrap_or("")
}

fn name_of(node: Node<'_>, src: &[u8], field: &str) -> Option<String> {
    let n = node.child_by_field_name(field)?;
    Some(node_text(n, src).trim().to_string())
}

/// Is this Rust function annotated `#[test]`, or a Python `test_*`?
fn is_test(
    spec: &LangSpec,
    node: Node<'_>,
    src: &[u8],
    simple_name: &str,
    scope: &[String],
) -> bool {
    if scope.iter().any(|s| s == "tests" || s == "test") {
        return true;
    }
    match spec.lang {
        crate::lang::Lang::Rust => {
            // Attributes are siblings before the item in rust's grammar.
            let mut sib = node.prev_sibling();
            let mut hops = 0;
            while let Some(s) = sib {
                if hops > 4 {
                    break;
                }
                if s.kind() == "attribute_item" {
                    let text = node_text(s, src);
                    if text.contains("test") {
                        return true;
                    }
                } else if s.kind() != "line_comment" && s.kind() != "block_comment" {
                    break;
                }
                sib = s.prev_sibling();
                hops += 1;
            }
            false
        }
        crate::lang::Lang::Python => simple_name.starts_with("test_"),
        crate::lang::Lang::TypeScript => {
            simple_name.starts_with("test") || simple_name.ends_with("Test")
        }
    }
}

fn collect(
    spec: &LangSpec,
    node: Node<'_>,
    src: &[u8],
    scope: &mut Vec<String>,
    out: &mut Vec<Entity>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let kind = child.kind();

        // Does this node open a naming scope (impl block, class, module)?
        //
        // A trait impl has to carry the trait in the scope name. `impl Debug for Digest` and
        // `impl Display for Digest` both define `fmt`, so scoping on the type alone gives two
        // different functions the same id - which is not merely a lint problem: an op naming
        // `fn:Digest::fmt` would be ambiguous, and impact analysis would conflate them. Found by
        // running this adapter over its own source.
        let opened = spec.container_name_field(kind).and_then(|field| {
            if kind == "impl_item" {
                let ty = name_of(child, src, "type")?;
                match name_of(child, src, "trait") {
                    Some(tr) => Some(format!("<{ty} as {tr}>")),
                    None => Some(ty),
                }
            } else {
                name_of(child, src, field)
            }
        });

        if let Some(entity_kind) = spec.entity_kind(kind)
            && let Some(simple) = name_of(child, src, "name")
        {
            let qualified = if scope.is_empty() {
                simple.clone()
            } else {
                format!("{}{}{}", scope.join(spec.sep), spec.sep, simple)
            };
            let test = is_test(spec, child, src, &simple, scope);
            let final_kind = if test && entity_kind == "fn" {
                "test"
            } else {
                entity_kind
            };
            let body = child
                .child_by_field_name("body")
                .map(|b| (b.start_byte(), b.end_byte()));
            let signature = {
                let sig_end = body.map(|(s, _)| s).unwrap_or(child.end_byte());
                let raw = std::str::from_utf8(&src[child.start_byte()..sig_end.min(src.len())])
                    .unwrap_or("")
                    .trim()
                    .to_string();
                Some(raw.lines().collect::<Vec<_>>().join(" ").trim().to_string())
            };
            out.push(Entity {
                id: format!("{final_kind}:{qualified}"),
                kind: final_kind.to_string(),
                start: child.start_byte(),
                end: child.end_byte(),
                body,
                signature,
                refs: Vec::new(),
                parent: if scope.is_empty() {
                    None
                } else {
                    Some(scope.join(spec.sep))
                },
            });
        }

        match opened {
            Some(name) => {
                scope.push(name);
                collect(spec, child, src, scope, out);
                scope.pop();
            }
            None => collect(spec, child, src, scope, out),
        }
    }
}

fn collect_imports(spec: &LangSpec, tree: &Tree, src: &[u8], out: &mut Vec<Entity>) {
    let mut cursor = tree.walk();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if spec.is_import(node.kind()) {
            let text = node_text(node, src).trim().to_string();
            out.push(Entity {
                id: format!("import:{text}"),
                kind: "import".to_string(),
                start: node.start_byte(),
                end: node.end_byte(),
                body: None,
                signature: Some(text),
                refs: Vec::new(),
                parent: None,
            });
            continue;
        }
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
}

/// Link entities by the identifiers that appear inside them.
fn resolve_refs(spec: &LangSpec, src: &[u8], entities: &mut [Entity]) {
    // simple name -> ids that define it. A name defined twice (an inherent method and a trait
    // method, say) links to both: over-linking is safe for impact, under-linking is not.
    let mut by_name: HashMap<String, Vec<String>> = HashMap::new();
    for e in entities.iter() {
        if e.kind == "import" {
            continue;
        }
        // An id is `kind:Qualified::name`; the linkable word is the last segment of the
        // qualified part, so strip the kind prefix before splitting on the scope separator.
        let qualified = e.id.split_once(':').map(|(_, r)| r).unwrap_or(&e.id);
        let simple = qualified.rsplit(spec.sep).next().unwrap_or(qualified);
        by_name
            .entry(simple.to_string())
            .or_default()
            .push(e.id.clone());
    }

    let mut found: Vec<(usize, BTreeSet<String>)> = Vec::new();
    for (idx, e) in entities.iter().enumerate() {
        let (bs, be) = e.body.unwrap_or((e.start, e.end));
        let text = std::str::from_utf8(&src[bs.min(src.len())..be.min(src.len())]).unwrap_or("");
        let mut refs = BTreeSet::new();
        for word in identifiers(text) {
            if let Some(ids) = by_name.get(word) {
                for id in ids {
                    if *id != e.id {
                        refs.insert(id.clone());
                    }
                }
            }
        }
        found.push((idx, refs));
    }

    for (idx, refs) in found {
        entities[idx].refs = refs.into_iter().collect();
    }
}

/// Identifier-shaped words. A tokenizer rather than a regex: fewer dependencies, and it is the
/// hot path when a repo is first indexed.
fn identifiers(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_alphabetic() || c == b'_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            out.push(&text[start..i]);
        } else {
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::LangSpec;

    fn rust_outline(code: &str) -> Outline {
        let spec = LangSpec::for_path("src/lib.rs").unwrap();
        outline(spec, &Artifact::new("src/lib.rs", code.as_bytes().to_vec())).unwrap()
    }

    #[test]
    fn functions_structs_and_impl_methods_get_qualified_ids() {
        let o = rust_outline(
            r#"
struct Header { name: String }

impl Header {
    fn parse(input: &str) -> Header { Header { name: input.to_string() } }
}

fn helper() -> u32 { 7 }
"#,
        );
        let ids = o.ids();
        assert!(ids.contains(&"struct:Header"), "{ids:?}");
        assert!(ids.contains(&"fn:Header::parse"), "{ids:?}");
        assert!(ids.contains(&"fn:helper"), "{ids:?}");
    }

    #[test]
    fn ids_survive_the_entity_moving_within_the_file() {
        let a = rust_outline("fn one() {}\nfn two() {}\n");
        let b = rust_outline("fn two() {}\nfn one() {}\n");
        let mut ids_a = a.ids();
        let mut ids_b = b.ids();
        ids_a.sort();
        ids_b.sort();
        assert_eq!(
            ids_a, ids_b,
            "reordering a file must not change any entity id"
        );
    }

    #[test]
    fn tests_are_recognised_by_attribute_and_by_module() {
        let o = rust_outline(
            r#"
fn parse() -> u32 { 1 }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_header() { assert_eq!(parse(), 1); }
}
"#,
        );
        let ids = o.ids();
        assert!(
            ids.iter().any(|i| i.starts_with("test:")),
            "no test entity found in {ids:?}"
        );
    }

    #[test]
    fn references_link_a_test_to_what_it_calls() {
        let o = rust_outline(
            r#"
fn parse() -> u32 { 1 }

#[test]
fn checks_parse() { assert_eq!(parse(), 1); }
"#,
        );
        let t = o
            .entities
            .iter()
            .find(|e| e.kind == "test")
            .expect("a test entity");
        assert!(
            t.refs.iter().any(|r| r == "fn:parse"),
            "test should reference fn:parse, got {:?}",
            t.refs
        );
    }

    #[test]
    fn imports_are_entities_too() {
        let o = rust_outline("use std::io::Write;\nfn f() {}\n");
        assert!(o.ids().iter().any(|i| i.starts_with("import:")));
    }

    #[test]
    fn syntax_errors_are_located_not_just_detected() {
        let spec = LangSpec::for_path("x.rs").unwrap();
        let art = Artifact::new("x.rs", b"fn broken( { }".to_vec());
        let tree = parse(spec, &art).unwrap();
        let errs = syntax_errors(&tree);
        assert!(!errs.is_empty(), "a broken file must report an error range");
    }

    #[test]
    fn python_functions_and_classes_parse() {
        let spec = LangSpec::for_path("app.py").unwrap();
        let code = "class Parser:\n    def parse(self):\n        return 1\n\ndef test_parse():\n    assert Parser().parse() == 1\n";
        let o = outline(spec, &Artifact::new("app.py", code.as_bytes().to_vec())).unwrap();
        let ids = o.ids();
        assert!(ids.contains(&"class:Parser"), "{ids:?}");
        assert!(ids.contains(&"fn:Parser.parse"), "{ids:?}");
        assert!(ids.contains(&"test:test_parse"), "{ids:?}");
    }

    #[test]
    fn typescript_classes_and_methods_parse() {
        let spec = LangSpec::for_path("app.ts").unwrap();
        let code = "export class Parser {\n  parse(s: string): number { return 1; }\n}\nfunction helper() { return 2; }\n";
        let o = outline(spec, &Artifact::new("app.ts", code.as_bytes().to_vec())).unwrap();
        let ids = o.ids();
        assert!(ids.contains(&"class:Parser"), "{ids:?}");
        assert!(ids.contains(&"fn:Parser.parse"), "{ids:?}");
        assert!(ids.contains(&"fn:helper"), "{ids:?}");
    }
}

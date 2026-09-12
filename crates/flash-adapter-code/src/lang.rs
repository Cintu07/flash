//! Which grammar handles which file, and what counts as an entity in each.
//!
//! One table per language rather than one parser per language. The entity extractor walks any
//! tree-sitter tree and consults this spec, so adding a language is a table entry plus a grammar
//! dependency, not another pass of bespoke traversal code.

use tree_sitter::Language;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    Rust,
    Python,
    TypeScript,
}

/// How to read one language's tree.
pub struct LangSpec {
    pub lang: Lang,
    pub name: &'static str,
    pub extensions: &'static [&'static str],
    /// tree-sitter node kind -> entity kind.
    pub items: &'static [(&'static str, &'static str)],
    /// Node kinds whose name qualifies the entities inside them: `impl Foo { fn bar }` is
    /// `Foo::bar`, not `bar`. Without this, two inherent methods with the same name in one file
    /// collide, and every id downstream is wrong.
    pub containers: &'static [(&'static str, &'static str)],
    /// Node kinds that are imports.
    pub imports: &'static [&'static str],
    /// Separator used when qualifying names.
    pub sep: &'static str,
    /// Braces or indentation: decides how a replaced body is re-wrapped.
    pub braced: bool,
}

const RUST: LangSpec = LangSpec {
    lang: Lang::Rust,
    name: "rust",
    extensions: &["rs"],
    items: &[
        ("function_item", "fn"),
        ("struct_item", "struct"),
        ("enum_item", "enum"),
        ("trait_item", "trait"),
        ("type_item", "type"),
        ("const_item", "const"),
        ("static_item", "static"),
        ("mod_item", "mod"),
        ("macro_definition", "macro"),
    ],
    // `type` is the field holding the name on an impl block; `name` on the rest.
    containers: &[
        ("impl_item", "type"),
        ("mod_item", "name"),
        ("trait_item", "name"),
    ],
    imports: &["use_declaration"],
    sep: "::",
    braced: true,
};

const PYTHON: LangSpec = LangSpec {
    lang: Lang::Python,
    name: "python",
    extensions: &["py"],
    items: &[("function_definition", "fn"), ("class_definition", "class")],
    containers: &[("class_definition", "name")],
    imports: &["import_statement", "import_from_statement"],
    sep: ".",
    braced: false,
};

const TYPESCRIPT: LangSpec = LangSpec {
    lang: Lang::TypeScript,
    name: "typescript",
    extensions: &["ts", "tsx", "mts", "cts"],
    items: &[
        ("function_declaration", "fn"),
        ("method_definition", "fn"),
        ("class_declaration", "class"),
        ("interface_declaration", "interface"),
        ("type_alias_declaration", "type"),
        ("enum_declaration", "enum"),
    ],
    containers: &[("class_declaration", "name")],
    imports: &["import_statement"],
    sep: ".",
    braced: true,
};

pub const ALL: &[&LangSpec] = &[&RUST, &PYTHON, &TYPESCRIPT];

impl LangSpec {
    pub fn for_path(path: &str) -> Option<&'static LangSpec> {
        let ext = path.rsplit('.').next()?.to_ascii_lowercase();
        ALL.iter()
            .copied()
            .find(|s| s.extensions.contains(&ext.as_str()))
    }

    pub fn language(&self) -> Language {
        match self.lang {
            Lang::Rust => Language::new(tree_sitter_rust::LANGUAGE),
            Lang::Python => Language::new(tree_sitter_python::LANGUAGE),
            Lang::TypeScript => Language::new(tree_sitter_typescript::LANGUAGE_TYPESCRIPT),
        }
    }

    pub fn entity_kind(&self, ts_kind: &str) -> Option<&'static str> {
        self.items
            .iter()
            .find(|(k, _)| *k == ts_kind)
            .map(|(_, e)| *e)
    }

    pub fn container_name_field(&self, ts_kind: &str) -> Option<&'static str> {
        self.containers
            .iter()
            .find(|(k, _)| *k == ts_kind)
            .map(|(_, f)| *f)
    }

    pub fn is_import(&self, ts_kind: &str) -> bool {
        self.imports.contains(&ts_kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_route_to_the_right_grammar() {
        assert_eq!(LangSpec::for_path("src/lib.rs").unwrap().name, "rust");
        assert_eq!(LangSpec::for_path("app/main.py").unwrap().name, "python");
        assert_eq!(
            LangSpec::for_path("web/App.tsx").unwrap().name,
            "typescript"
        );
        assert!(LangSpec::for_path("README.md").is_none());
        assert!(LangSpec::for_path("Makefile").is_none());
    }

    #[test]
    fn every_grammar_loads() {
        for spec in ALL {
            let mut parser = tree_sitter::Parser::new();
            parser
                .set_language(&spec.language())
                .unwrap_or_else(|e| panic!("{} grammar failed to load: {e}", spec.name));
        }
    }
}

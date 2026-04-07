use tower_lsp::lsp_types::{Position, Range};
use tree_sitter::{Parser, Query, QueryCursor, StreamingIterator, Tree};

pub struct SrcTree {
    src: String,
    tree: Tree,
}

impl SrcTree {
    pub fn new(src: String) -> Self {
        let language = tree_sitter_tonel_smalltalk::language();
        let mut parser = Parser::new();
        parser
            .set_language(&language)
            .expect("Error loading tonel_smalltalk grammar");
        let tree = parser
            .parse(&src, None)
            .expect("Error parsing tonel_smalltalk source");
        Self { src, tree }
    }

    pub fn src(&self) -> &str {
        &self.src
    }

    pub fn tree(&self) -> &Tree {
        &self.tree
    }

    /// Returns the class or trait name defined in this file and the LSP Range of
    /// its definition node, or None if this file does not contain a definition.
    pub fn defined_class(&self) -> Option<(String, Range)> {
        let language = tree_sitter_tonel_smalltalk::language();

        // Match class_definition or trait_definition and capture the #name value.
        // Note: r##"..."## is required because the pattern contains "#name" which
        // would prematurely terminate r#"..."#.
        let query = Query::new(
            &language,
            r##"[
                (class_definition (ston_map (ston_pair
                    (ston_key (ston_symbol) @key (#eq? @key "#name"))
                    (ston_value (ston_symbol) @name))))
                (class_definition (ston_map (ston_pair
                    (ston_key (ston_symbol) @key (#eq? @key "#name"))
                    (ston_value (string) @name))))
                (trait_definition (ston_map (ston_pair
                    (ston_key (ston_symbol) @key (#eq? @key "#name"))
                    (ston_value (ston_symbol) @name))))
                (trait_definition (ston_map (ston_pair
                    (ston_key (ston_symbol) @key (#eq? @key "#name"))
                    (ston_value (string) @name))))
            ]"##,
        )
        .ok()?;

        let name_idx = query.capture_index_for_name("name")?;

        let mut cursor = QueryCursor::new();
        let mut captures = cursor.captures(&query, self.tree.root_node(), self.src.as_bytes());
        let (m, _) = captures.next()?;

        let name_node = m.captures.iter().find(|c| c.index == name_idx)?.node;

        let raw = name_node.utf8_text(self.src.as_bytes()).ok()?;
        let class_name = if raw.starts_with('#') {
            raw.trim_start_matches('#').to_string()
        } else {
            // string format: 'ClassName'
            raw.trim_matches('\'').to_string()
        };

        // Walk up to find the enclosing class_definition or trait_definition node
        // in order to get its Range for go-to-definition.
        let mut def_node = name_node;
        loop {
            match def_node.kind() {
                "class_definition" | "trait_definition" => break,
                _ => def_node = def_node.parent()?,
            }
        }

        let start = def_node.start_position();
        let end = def_node.end_position();
        Some((
            class_name,
            Range {
                start: Position {
                    line: start.row as u32,
                    character: start.column as u32,
                },
                end: Position {
                    line: end.row as u32,
                    character: end.column as u32,
                },
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_class_definition_extracts_name() {
        let src = "Class { #name: #Foo, #superclass: #Object }";
        let tree = SrcTree::new(src.to_string());
        let result = tree.defined_class();
        assert!(result.is_some());
        let (name, _range) = result.unwrap();
        assert_eq!(name, "Foo");
    }

    #[test]
    fn test_class_definition_with_string_name() {
        let src = "Class { #name : 'Foo', #superclass : 'Object' }";
        let tree = SrcTree::new(src.to_string());
        let result = tree.defined_class();
        assert!(result.is_some());
        let (name, _range) = result.unwrap();
        assert_eq!(name, "Foo");
    }

    #[test]
    fn test_trait_definition_extracts_name() {
        let src = "Trait { #name: #MyTrait }";
        let tree = SrcTree::new(src.to_string());
        let result = tree.defined_class();
        assert!(result.is_some());
        let (name, _range) = result.unwrap();
        assert_eq!(name, "MyTrait");
    }

    #[test]
    fn test_method_only_file_returns_none() {
        let src = "Foo >> bar [\n    ^42\n]";
        let tree = SrcTree::new(src.to_string());
        assert!(tree.defined_class().is_none());
    }
}

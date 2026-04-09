use std::collections::HashMap;

use tower_lsp::lsp_types::{Position, Range};
use tree_sitter::{Parser, Query, QueryCursor, StreamingIterator, Tree};

/// Result of resolving the token at a cursor position to a class name.
pub enum ClassAtPos {
    NotNode,
    NotIdentifier(String),
    NotUppercase(String),
    Found(String),
}

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

    /// Resolves the token at `pos` to a class name (uppercase-first identifier/symbol/string).
    pub fn class_at_position(&self, pos: &Position) -> ClassAtPos {
        let point = tree_sitter::Point {
            row: pos.line as usize,
            column: pos.character as usize,
        };
        let is_token =
            |k: &str| matches!(k, "identifier" | "string" | "symbol" | "ston_symbol");
        let root = self.tree.root_node();
        let node = root.descendant_for_point_range(point, point).and_then(|n| {
            if !is_token(n.kind()) && point.column > 0 {
                let prev = tree_sitter::Point { row: point.row, column: point.column - 1 };
                root.descendant_for_point_range(prev, prev)
            } else {
                Some(n)
            }
        });
        let Some(n) = node else {
            return ClassAtPos::NotNode;
        };
        let kind = n.kind();
        if !is_token(kind) {
            return ClassAtPos::NotIdentifier(kind.to_string());
        }
        match n.utf8_text(self.src.as_bytes()) {
            Ok(raw) => {
                let s = normalize_class_name(raw);
                if s.starts_with(|c: char| c.is_uppercase()) {
                    ClassAtPos::Found(s.to_string())
                } else {
                    ClassAtPos::NotUppercase(s.to_string())
                }
            }
            Err(_) => ClassAtPos::NotNode,
        }
    }

    /// Returns all class-name occurrences in this file, grouped by name.
    /// Covers `ClassName`, `#ClassName`, `#'ClassName'`, and `'ClassName'` forms.
    pub fn all_class_references(&self) -> HashMap<String, Vec<Range>> {
        let mut map: HashMap<String, Vec<Range>> = HashMap::new();
        let root_node = self.tree.root_node();
        let mut cursor = root_node.walk();

        'outer: loop {
            let node = cursor.node();
            let kind = node.kind();
            if matches!(kind, "identifier" | "string" | "symbol" | "ston_symbol") {
                if let Ok(raw) = node.utf8_text(self.src.as_bytes()) {
                    let name = normalize_class_name(raw);
                    if name.starts_with(|c: char| c.is_uppercase()) {
                        let start = node.start_position();
                        let end = node.end_position();
                        map.entry(name.to_string()).or_default().push(Range {
                            start: Position {
                                line: start.row as u32,
                                character: start.column as u32,
                            },
                            end: Position {
                                line: end.row as u32,
                                character: end.column as u32,
                            },
                        });
                    }
                }
            }

            if cursor.goto_first_child() {
                continue;
            }
            loop {
                if cursor.goto_next_sibling() {
                    break;
                }
                if !cursor.goto_parent() {
                    break 'outer;
                }
            }
        }

        map
    }

    /// Returns all LSP Ranges where `class_name` appears.
    pub fn find_class_references(&self, class_name: &str) -> Vec<Range> {
        self.all_class_references().remove(class_name).unwrap_or_default()
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
        let class_name = normalize_class_name(raw).to_string();

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

/// Strip Tonel name decoration and return a plain class name.
///
/// Handles all three forms that Tonel allows for `#name`:
///   `#ClassName`    -> `ClassName`
///   `#'ClassName'`  -> `ClassName`
///   `'ClassName'`   -> `ClassName`
///   `ClassName`     -> `ClassName` (identifier, no-op)
pub fn normalize_class_name(raw: &str) -> &str {
    raw.trim_start_matches('#').trim_matches('\'')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_fixture_class(relative_path: &str, expected: &str) {
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(relative_path);
        let src = std::fs::read_to_string(&fixture).expect("fixture not found");
        let tree = SrcTree::new(src);
        let result = tree.defined_class();
        assert!(result.is_some(), "expected class definition in {relative_path}");
        assert_eq!(result.unwrap().0, expected);
    }

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
    fn test_class_definition_with_quoted_symbol_name() {
        let src = "Class { #name : #'Foo', #superclass : #Object }";
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

    #[test]
    fn test_fixture_string_name_class() {
        assert_fixture_class("test/tonel/Dummy-Core/DmError.class.st", "DmError");
    }

    #[test]
    fn test_fixture_symbol_name_class() {
        assert_fixture_class("test/tonel/Dummy-Core/DmSymbolName.class.st", "DmSymbolName");
    }

    #[test]
    fn test_find_class_references_all_forms() {
        // Method file referencing a class in all three forms.
        let src = "Foo >> bar [\n    | a b c |\n    a := #ClassName new.\n    b := #'ClassName' new.\n    c := 'ClassName' new.\n    ^a\n]";
        let tree = SrcTree::new(src.to_string());
        let refs = tree.find_class_references("ClassName");
        assert_eq!(refs.len(), 3, "expected 3 references, got {:?}", refs);
    }

    #[test]
    fn test_find_class_references_in_class_definition() {
        // Class definition file — the class name itself and superclass are both references.
        let src = "Class { #name: #Foo, #superclass: #Object }";
        let tree = SrcTree::new(src.to_string());
        let refs = tree.find_class_references("Foo");
        assert!(!refs.is_empty(), "expected at least one reference to Foo");
    }

    #[test]
    fn test_find_class_references_no_match() {
        let src = "Foo >> bar [ ^#Bar new ]";
        let tree = SrcTree::new(src.to_string());
        assert!(tree.find_class_references("Baz").is_empty());
    }

    #[test]
    fn test_fixture_quoted_symbol_name_class() {
        assert_fixture_class(
            "test/tonel/Dummy-Core/DmQuotedSymbolName.class.st",
            "DmQuotedSymbolName",
        );
    }
}

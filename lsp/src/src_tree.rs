use std::collections::HashMap;

use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};
use tree_sitter::{Node, Parser, Query, QueryCursor, StreamingIterator, Tree};

/// Metadata extracted from a class or trait definition file.
#[derive(Clone, Debug)]
pub struct ClassInfo {
    pub kind: String, // "Class" or "Trait"
    pub name: String,
    pub superclass: Option<String>,
    pub inst_vars: Vec<String>,
    pub class_vars: Vec<String>,
    pub class_inst_vars: Vec<String>,
}

/// Result of resolving the token at a cursor position to a class name.
pub enum ClassAtPos {
    NotNode,
    NotIdentifier(String),
    NotUppercase(String),
    Found(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MethodSide {
    Instance,
    Class,
}

pub struct SrcTree {
    src: String,
    tree: Tree,
}

impl SrcTree {
    fn token_node_at_position(&self, pos: &Position) -> Option<Node<'_>> {
        let point = tree_sitter::Point {
            row: pos.line as usize,
            column: pos.character as usize,
        };
        let root = self.tree.root_node();
        let is_token = |k: &str| matches!(k, "identifier" | "string" | "symbol" | "ston_symbol");
        root.descendant_for_point_range(point, point).and_then(|n| {
            if !is_token(n.kind()) && point.column > 0 {
                let prev = tree_sitter::Point {
                    row: point.row,
                    column: point.column - 1,
                };
                root.descendant_for_point_range(prev, prev)
            } else {
                Some(n)
            }
        })
    }

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
        let is_token = |k: &str| matches!(k, "identifier" | "string" | "symbol" | "ston_symbol");
        let node = self.token_node_at_position(pos);
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

    /// Returns method side at cursor position if inside a method definition.
    pub fn method_side_at_position(&self, pos: &Position) -> Option<MethodSide> {
        let node = self.token_node_at_position(pos)?;

        let mut cur = node;
        loop {
            if cur.kind() == "method_definition" {
                break;
            }
            cur = cur.parent()?;
        }

        let method_ref = (0..cur.named_child_count())
            .filter_map(|i| cur.named_child(i as u32))
            .find(|n| n.kind() == "method_reference")?;
        if method_ref.child_by_field_name("class_side").is_some() {
            Some(MethodSide::Class)
        } else {
            Some(MethodSide::Instance)
        }
    }

    /// Returns true when the identifier at `pos` is a local temporary/argument.
    pub fn is_local_variable_at_position(&self, pos: &Position) -> bool {
        let Some(node) = self.token_node_at_position(pos) else {
            return false;
        };
        if node.kind() != "identifier" {
            return false;
        }
        let Ok(name) = node.utf8_text(self.src.as_bytes()) else {
            return false;
        };

        // Check nearest block scopes first (shadowing-aware, nearest wins).
        let mut cur = Some(node);
        while let Some(n) = cur {
            if n.kind() == "block" && block_declares_identifier(n, name, self.src.as_bytes()) {
                return true;
            }
            if n.kind() == "method_definition" {
                return method_declares_identifier(n, name, self.src.as_bytes());
            }
            cur = n.parent();
        }
        false
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
        self.all_class_references()
            .remove(class_name)
            .unwrap_or_default()
    }

    /// Returns all syntax error diagnostics derived from the tree-sitter parse result.
    /// Uses ERROR nodes (unexpected tokens) and MISSING nodes (absent expected tokens).
    pub fn syntax_errors(&self) -> Vec<Diagnostic> {
        let root = self.tree.root_node();
        if !root.has_error() {
            return Vec::new();
        }

        let mut diagnostics = Vec::new();
        let mut cursor = root.walk();

        'outer: loop {
            let node = cursor.node();

            if node.is_missing() {
                diagnostics.push(make_error_diagnostic(
                    node_to_range(&node),
                    format!("Missing '{}'", node.kind()),
                ));
            } else if node.is_error() {
                let raw = node.utf8_text(self.src.as_bytes()).unwrap_or("").trim();
                let msg = if raw.is_empty() {
                    "Syntax error".to_string()
                } else {
                    let display: String = raw.chars().take(50).collect();
                    let suffix = if raw.chars().count() > 50 { "..." } else { "" };
                    format!("Syntax error: unexpected '{}{}'", display, suffix)
                };
                diagnostics.push(make_error_diagnostic(node_to_range(&node), msg));
                // Skip children of ERROR nodes to avoid cascading reports.
                loop {
                    if cursor.goto_next_sibling() {
                        break;
                    }
                    if !cursor.goto_parent() {
                        break 'outer;
                    }
                }
                continue;
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

        diagnostics
    }

    /// Returns metadata for the class or trait defined in this file, or None if none.
    pub fn class_info(&self) -> Option<ClassInfo> {
        let root = self.tree.root_node();

        // Tree: source_file → definition → class_definition/trait_definition
        let def_node = find_class_or_trait_node(root)?;

        let kind = if def_node.kind() == "class_definition" {
            "Class"
        } else {
            "Trait"
        };

        let ston_map = (0..def_node.named_child_count())
            .filter_map(|i| def_node.named_child(i as u32))
            .find(|n| n.kind() == "ston_map")?;

        let mut name: Option<String> = None;
        let mut superclass: Option<String> = None;
        let mut inst_vars = Vec::new();
        let mut class_vars = Vec::new();
        let mut class_inst_vars = Vec::new();

        for i in 0..ston_map.named_child_count() {
            let Some(pair) = ston_map.named_child(i as u32) else {
                continue;
            };
            if pair.kind() != "ston_pair" {
                continue;
            }

            let key_node = (0..pair.named_child_count())
                .filter_map(|j| pair.named_child(j as u32))
                .find(|n| n.kind() == "ston_key");
            let val_node = (0..pair.named_child_count())
                .filter_map(|j| pair.named_child(j as u32))
                .find(|n| n.kind() == "ston_value");
            let (Some(kn), Some(vn)) = (key_node, val_node) else {
                continue;
            };

            let key: &str = (0..kn.named_child_count())
                .filter_map(|j| kn.named_child(j as u32))
                .next()
                .and_then(|s| s.utf8_text(self.src.as_bytes()).ok())
                .map(normalize_class_name)
                .unwrap_or("");

            let Some(actual) = (0..vn.named_child_count())
                .filter_map(|j| vn.named_child(j as u32))
                .next()
            else {
                continue;
            };

            match key {
                "name" => {
                    if let Ok(raw) = actual.utf8_text(self.src.as_bytes()) {
                        name = Some(normalize_class_name(raw).to_string());
                    }
                }
                "superclass" => {
                    if let Ok(raw) = actual.utf8_text(self.src.as_bytes()) {
                        let sc = normalize_class_name(raw);
                        if !sc.is_empty() {
                            superclass = Some(sc.to_string());
                        }
                    }
                }
                "instVars" => {
                    inst_vars = extract_ston_array_strings(actual, self.src.as_bytes());
                }
                "classVars" => {
                    class_vars = extract_ston_array_strings(actual, self.src.as_bytes());
                }
                "classInstVars" => {
                    class_inst_vars = extract_ston_array_strings(actual, self.src.as_bytes());
                }
                _ => {}
            }
        }

        Some(ClassInfo {
            kind: kind.to_string(),
            name: name?,
            superclass,
            inst_vars,
            class_vars,
            class_inst_vars,
        })
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

fn node_to_range(node: &Node) -> Range {
    let start = node.start_position();
    let end = node.end_position();
    Range {
        start: Position {
            line: start.row as u32,
            character: start.column as u32,
        },
        end: Position {
            line: end.row as u32,
            character: end.column as u32,
        },
    }
}

fn make_error_diagnostic(range: Range, message: String) -> Diagnostic {
    Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::ERROR),
        source: Some("tonel-smalltalk".to_string()),
        message,
        ..Default::default()
    }
}

/// Finds the first `class_definition` or `trait_definition` node via DFS.
fn find_class_or_trait_node(node: tree_sitter::Node) -> Option<tree_sitter::Node> {
    if matches!(node.kind(), "class_definition" | "trait_definition") {
        return Some(node);
    }
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i as u32) {
            if let Some(found) = find_class_or_trait_node(child) {
                return Some(found);
            }
        }
    }
    None
}

/// Extracts string values from a `ston_list` node (e.g., instVars list).
/// Tree: ston_list → ston_value → (string | ston_symbol | identifier)
fn extract_ston_array_strings(node: tree_sitter::Node, src: &[u8]) -> Vec<String> {
    if node.kind() != "ston_list" {
        return Vec::new();
    }
    (0..node.named_child_count())
        .filter_map(|i| node.named_child(i as u32))
        // each child is a ston_value; get its first named child (the actual value)
        .filter_map(|sv| {
            if sv.kind() == "ston_value" {
                sv.named_child(0)
            } else {
                Some(sv)
            }
        })
        .filter_map(|n| n.utf8_text(src).ok())
        .map(|raw| normalize_class_name(raw).to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn node_has_identifier_named(node: Node<'_>, name: &str, src: &[u8]) -> bool {
    if node.kind() == "identifier" {
        return node.utf8_text(src).ok() == Some(name);
    }
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i as u32) {
            if node_has_identifier_named(child, name, src) {
                return true;
            }
        }
    }
    false
}

fn block_declares_identifier(block_node: Node<'_>, name: &str, src: &[u8]) -> bool {
    for i in 0..block_node.named_child_count() {
        let Some(child) = block_node.named_child(i as u32) else {
            continue;
        };
        if matches!(child.kind(), "block_argument" | "temporaries")
            && node_has_identifier_named(child, name, src)
        {
            return true;
        }
    }
    false
}

fn method_declares_identifier(method_node: Node<'_>, name: &str, src: &[u8]) -> bool {
    for i in 0..method_node.named_child_count() {
        let Some(child) = method_node.named_child(i as u32) else {
            continue;
        };
        if child.kind() == "method_reference" {
            let mut walk = child.walk();
            for param in child.children_by_field_name("param", &mut walk) {
                if node_has_identifier_named(param, name, src) {
                    return true;
                }
            }
        }
        if child.kind() == "method_body" {
            for j in 0..child.named_child_count() {
                let Some(body_child) = child.named_child(j as u32) else {
                    continue;
                };
                if body_child.kind() == "temporaries"
                    && node_has_identifier_named(body_child, name, src)
                {
                    return true;
                }
            }
        }
    }
    false
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
        assert!(
            result.is_some(),
            "expected class definition in {relative_path}"
        );
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
        assert_fixture_class(
            "test/tonel/Dummy-Core/DmSymbolName.class.st",
            "DmSymbolName",
        );
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

    #[test]
    fn test_syntax_errors_clean_class_definition() {
        let src = "Class { #name: #Foo, #superclass: #Object }";
        let tree = SrcTree::new(src.to_string());
        assert!(tree.syntax_errors().is_empty());
    }

    #[test]
    fn test_syntax_errors_clean_fixture() {
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("test/tonel/Dummy-Core/DmError.class.st");
        let src = std::fs::read_to_string(&fixture).expect("fixture not found");
        let tree = SrcTree::new(src);
        assert!(tree.syntax_errors().is_empty());
    }

    #[test]
    fn test_syntax_errors_unclosed_brace() {
        let src = "Class { #name: #Foo, #superclass: #Object";
        let tree = SrcTree::new(src.to_string());
        let errors = tree.syntax_errors();
        assert!(!errors.is_empty(), "expected at least one diagnostic");
        assert!(errors
            .iter()
            .all(|d| d.severity == Some(DiagnosticSeverity::ERROR)));
    }

    #[test]
    fn test_syntax_errors_source_field() {
        let src = "!!!garbage!!!";
        let tree = SrcTree::new(src.to_string());
        let errors = tree.syntax_errors();
        assert!(!errors.is_empty());
        for d in &errors {
            assert_eq!(d.source.as_deref(), Some("tonel-smalltalk"));
        }
    }

    #[test]
    fn test_class_info_basic() {
        let src = "Class { #name: #Foo, #superclass: #Object }";
        let tree = SrcTree::new(src.to_string());
        let info = tree.class_info().unwrap();
        assert_eq!(info.kind, "Class");
        assert_eq!(info.name, "Foo");
        assert_eq!(info.superclass.as_deref(), Some("Object"));
        assert!(info.inst_vars.is_empty());
        assert!(info.class_vars.is_empty());
        assert!(info.class_inst_vars.is_empty());
    }

    #[test]
    fn test_class_info_with_inst_vars() {
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("test/tonel/Dummy-Core/DmGenericError.class.st");
        let src = std::fs::read_to_string(&fixture).unwrap();
        let tree = SrcTree::new(src);
        let info = tree.class_info().unwrap();
        assert_eq!(info.kind, "Class");
        assert_eq!(info.name, "DmSubError");
        assert_eq!(info.superclass.as_deref(), Some("DmError"));
        assert!(
            info.inst_vars.contains(&"errorPrefix".to_string()),
            "expected errorPrefix in inst_vars: {:?}",
            info.inst_vars
        );
    }

    #[test]
    fn test_class_info_trait() {
        let src = "Trait { #name: #MyTrait }";
        let tree = SrcTree::new(src.to_string());
        let info = tree.class_info().unwrap();
        assert_eq!(info.kind, "Trait");
        assert_eq!(info.name, "MyTrait");
        assert!(info.superclass.is_none());
    }

    #[test]
    fn test_class_info_method_file_returns_none() {
        let src = "Foo >> bar [ ^42 ]";
        let tree = SrcTree::new(src.to_string());
        assert!(tree.class_info().is_none());
    }

    #[test]
    fn test_class_info_with_class_inst_vars() {
        let src = "Class { #name: #Foo, #superclass: #Object, #classInstVars: ['x'] }";
        let tree = SrcTree::new(src.to_string());
        let info = tree.class_info().unwrap();
        assert_eq!(info.name, "Foo");
        assert_eq!(info.class_inst_vars, vec!["x".to_string()]);
    }

    #[test]
    fn test_method_side_at_position() {
        let src = "Class { #name: #Foo, #superclass: #Object }\n\nFoo class >> bar [\n    ^x\n]\n\nFoo >> baz [\n    ^x\n]";
        let tree = SrcTree::new(src.to_string());
        let class_side = tree.method_side_at_position(&Position {
            line: 3,
            character: 5,
        });
        let instance_side = tree.method_side_at_position(&Position {
            line: 7,
            character: 5,
        });
        assert_eq!(class_side, Some(MethodSide::Class));
        assert_eq!(instance_side, Some(MethodSide::Instance));
    }

    #[test]
    fn test_is_local_variable_for_method_temporary() {
        let src = "Class { #name: #ClassB, #superclass: #Object }\n\nClassB >> methodA [\n    | varA |\n    varA := ClassA new.\n]";
        let tree = SrcTree::new(src.to_string());
        let is_local = tree.is_local_variable_at_position(&Position {
            line: 4,
            character: 6,
        });
        assert!(is_local);
    }

    #[test]
    fn test_is_local_variable_for_block_temporary() {
        let src = "Class { #name: #ClassB, #superclass: #Object }\n\nClassB class >> methodB [\n    blockA := [ | varA |\n        varA := ClassA new.\n    ].\n]";
        let tree = SrcTree::new(src.to_string());
        let is_local = tree.is_local_variable_at_position(&Position {
            line: 4,
            character: 8,
        });
        assert!(is_local);
    }
}

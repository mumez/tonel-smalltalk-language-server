use dashmap::DashMap;
use std::path::Path;
use tower_lsp::lsp_types::{Location, Range, Url};
use walkdir::WalkDir;

use crate::src_tree::SrcTree;

pub struct Workspace {
    /// Maps class_name -> (defining file URL, definition Range)
    class_index: DashMap<String, (Url, Range)>,
    /// Maps class_name -> all locations where that name appears across the workspace
    references_index: DashMap<String, Vec<Location>>,
    /// Maps file URL -> class names referenced in that file (for incremental update)
    file_refs: DashMap<Url, Vec<String>>,
}

impl Workspace {
    pub fn new() -> Self {
        Self {
            class_index: DashMap::new(),
            references_index: DashMap::new(),
            file_refs: DashMap::new(),
        }
    }

    /// Walks all `.st` files under `root`, parses each with SrcTree,
    /// and populates the indexes. Returns the number of classes indexed.
    pub fn scan(&self, root: &Path) -> anyhow::Result<usize> {
        let before = self.class_index.len();
        for (url, src_tree) in walk_st_files(root) {
            self.update(url, &src_tree);
        }
        Ok(self.class_index.len() - before)
    }

    /// Returns the number of classes currently in the index.
    pub fn class_count(&self) -> usize {
        self.class_index.len()
    }

    /// Updates the indexes for a single document (call on did_open/did_change).
    pub fn update(&self, url: Url, src_tree: &SrcTree) {
        if let Some((class_name, range)) = src_tree.defined_class() {
            self.class_index.insert(class_name, (url.clone(), range));
        }

        // Remove stale references from this file.
        if let Some((_, old_classes)) = self.file_refs.remove(&url) {
            for class_name in &old_classes {
                if let Some(mut locs) = self.references_index.get_mut(class_name) {
                    locs.retain(|loc| loc.uri != url);
                }
            }
        }

        // Add new references from this file.
        let all_refs = src_tree.all_class_references();
        if !all_refs.is_empty() {
            let mut referenced = Vec::with_capacity(all_refs.len());
            for (class_name, ranges) in all_refs {
                self.references_index
                    .entry(class_name.clone())
                    .or_default()
                    .extend(ranges.into_iter().map(|range| Location {
                        uri: url.clone(),
                        range,
                    }));
                referenced.push(class_name);
            }
            self.file_refs.insert(url, referenced);
        }
    }

    /// Looks up a class name and returns its definition location, or None if unknown.
    pub fn find_class(&self, name: &str) -> Option<(Url, Range)> {
        self.class_index.get(name).map(|entry| entry.value().clone())
    }

    /// Returns all locations where `class_name` appears across the workspace.
    pub fn find_references(&self, class_name: &str) -> Vec<Location> {
        self.references_index
            .get(class_name)
            .map(|locs| locs.value().clone())
            .unwrap_or_default()
    }
}

/// Yields `(Url, SrcTree)` for every `.st` file under `root`.
fn walk_st_files(root: &Path) -> impl Iterator<Item = (Url, SrcTree)> + '_ {
    WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_type().is_file()
                && e.path().extension().and_then(|s| s.to_str()) == Some("st")
        })
        .filter_map(|e| {
            let path = e.path().to_owned();
            let content = std::fs::read_to_string(&path).ok()?;
            let url = Url::from_file_path(&path).ok()?;
            Some((url, SrcTree::new(content)))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_scan_indexes_class_files() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("Foo.st"),
            "Class { #name: #Foo, #superclass: #Object }",
        )
        .unwrap();
        std::fs::write(dir.path().join("Bar.st"), "Trait { #name: #Bar }").unwrap();

        let workspace = Workspace::new();
        workspace.scan(dir.path()).unwrap();

        assert!(workspace.find_class("Foo").is_some());
        assert!(workspace.find_class("Bar").is_some());
        assert!(workspace.find_class("Baz").is_none());
    }

    #[test]
    fn test_scan_ignores_method_files() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("method.st"), "Foo >> bar [\n    ^42\n]").unwrap();

        let workspace = Workspace::new();
        workspace.scan(dir.path()).unwrap();

        assert!(workspace.find_class("Foo").is_none());
    }

    #[test]
    fn test_scan_fixture_directory() {
        let fixture_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("test/tonel/Dummy-Core");
        let workspace = Workspace::new();
        let count = workspace.scan(&fixture_dir).unwrap();
        assert!(count >= 4, "expected at least 4 classes, got {}", count);
        assert!(workspace.find_class("DmError").is_some());
        assert!(workspace.find_class("DmSymbolName").is_some());
        assert!(workspace.find_class("DmQuotedSymbolName").is_some());
    }

    #[test]
    fn test_find_references_across_files() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("Foo.st"),
            "Class { #name: #Foo, #superclass: #Object }",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("method.st"),
            "SomeClass >> bar [\n    | a b c |\n    a := #Foo new.\n    b := #'Foo' new.\n    c := 'Foo' new.\n    ^a\n]",
        )
        .unwrap();

        let workspace = Workspace::new();
        workspace.scan(dir.path()).unwrap();
        let locations = workspace.find_references("Foo");
        // Foo.st has 1 occurrence (#Foo in #name value), method.st has 3
        assert!(locations.len() >= 4, "expected at least 4 locations, got {}", locations.len());
    }

    #[test]
    fn test_update_indexes_single_document() {
        let workspace = Workspace::new();
        let url = Url::parse("file:///tmp/Foo.st").unwrap();
        let src_tree = SrcTree::new("Class { #name: #Foo, #superclass: #Object }".to_string());

        workspace.update(url.clone(), &src_tree);

        let result = workspace.find_class("Foo");
        assert!(result.is_some());
        let (found_url, _range) = result.unwrap();
        assert_eq!(found_url, url);
    }

    #[test]
    fn test_update_refreshes_references() {
        let workspace = Workspace::new();
        let url = Url::parse("file:///tmp/method.st").unwrap();

        let src_v1 = SrcTree::new("Foo >> bar [ ^#Bar new ]".to_string());
        workspace.update(url.clone(), &src_v1);
        assert!(!workspace.find_references("Bar").is_empty());
        assert!(workspace.find_references("Baz").is_empty());

        // Update the file: Bar replaced by Baz
        let src_v2 = SrcTree::new("Foo >> bar [ ^#Baz new ]".to_string());
        workspace.update(url.clone(), &src_v2);
        assert!(workspace.find_references("Bar").is_empty(), "stale Bar ref should be removed");
        assert!(!workspace.find_references("Baz").is_empty());
    }
}

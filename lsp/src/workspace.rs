use dashmap::DashMap;
use std::path::Path;
use tower_lsp::lsp_types::{Range, Url};
use walkdir::WalkDir;

use crate::src_tree::SrcTree;

pub struct Workspace {
    /// Maps class_name -> (defining file URL, definition Range)
    class_index: DashMap<String, (Url, Range)>,
}

impl Workspace {
    pub fn new() -> Self {
        Self {
            class_index: DashMap::new(),
        }
    }

    /// Walks all `.st` files under `root`, parses each with SrcTree,
    /// and populates the class index. Returns the number of classes indexed.
    pub fn scan(&self, root: &Path) -> anyhow::Result<usize> {
        let before = self.class_index.len();
        for entry in WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_type().is_file()
                    && e.path().extension().and_then(|s| s.to_str()) == Some("st")
            })
        {
            let path = entry.path();
            if let Ok(content) = std::fs::read_to_string(path) {
                if let Ok(url) = Url::from_file_path(path) {
                    let src_tree = SrcTree::new(content);
                    self.update(url, &src_tree);
                }
            }
        }
        Ok(self.class_index.len() - before)
    }

    /// Returns the number of classes currently in the index.
    pub fn class_count(&self) -> usize {
        self.class_index.len()
    }

    /// Updates the index entry for a single document (call on did_open/did_change).
    pub fn update(&self, url: Url, src_tree: &SrcTree) {
        if let Some((class_name, range)) = src_tree.defined_class() {
            self.class_index.insert(class_name, (url, range));
        }
    }

    /// Looks up a class name and returns its location, or None if unknown.
    pub fn find_class(&self, name: &str) -> Option<(Url, Range)> {
        self.class_index.get(name).map(|entry| entry.value().clone())
    }
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
}

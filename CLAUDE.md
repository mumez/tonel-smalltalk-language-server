# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

A Rust implementation of a Language Server Protocol (LSP) server for [Tonel-Smalltalk](https://github.com/mumez/tree-sitter-tonel-smalltalk), targeting the [Zed](https://zed.dev) editor. Supports go-to-definition and find-all-references for class names.

## Build & Test

```bash
# Build release binary
cargo build --release

# Run all tests
cargo test

# Run tests for a specific module
cargo test -p tonel-smalltalk-language-server src_tree
```

The binary is produced at `target/release/tonel-smalltalk-language-server`. Install it via:
```bash
cp target/release/tonel-smalltalk-language-server ~/.local/bin/
```

## Key Dependency

`tree-sitter-tonel-smalltalk` is fetched from GitHub (`https://github.com/mumez/tree-sitter-tonel-smalltalk`). The grammar provides the tree-sitter language parser for `.st` files.

## Architecture

All source lives in `lsp/src/` with four modules:

- **`main.rs`** — Entry point. Wires `tower-lsp` stdin/stdout transport with `Backend`.
- **`backend.rs`** — `Backend` struct implementing `tower_lsp::LanguageServer`. Handles LSP lifecycle (`initialize`, `initialized`, `shutdown`) and document events (`did_open`, `did_change`, `did_close`, `goto_definition`, `references`). Owns a `DashMap<Url, SrcTree>` for open documents and an `Arc<Workspace>`.
- **`src_tree.rs`** — `SrcTree` wraps a parsed tree-sitter `Tree` with its source string. `defined_class()` uses a tree-sitter query to extract the `#name` from `class_definition` or `trait_definition` nodes, returning the class/trait name and its LSP `Range`. `find_class_references()` walks the entire tree to find all occurrences of a class name in any token form.
- **`workspace.rs`** — `Workspace` maintains a `DashMap<String, (Url, Range)>` mapping class names to their defining file locations. `scan()` walks all `.st` files at startup; `update()` is called on each document open/change. `find_references()` re-walks all `.st` files to collect every reference to a given class name.

### Data Flow for Go-to-Definition

1. On `initialized`: `Workspace::scan()` runs in a blocking task, indexing all `.st` files.
2. On `did_open`/`did_change`: `Backend::on_change()` parses the document into a `SrcTree`, calls `Workspace::update()` to refresh the index, and stores the `SrcTree` in `document_map`.
3. On `goto_definition`: the backend finds the identifier node at the cursor position (uppercase-first = class name), then calls `Workspace::find_class()` to return the location.

### Data Flow for Find-All-References

1. On `references`: the backend identifies the class name at the cursor (same logic as `goto_definition`).
2. `Workspace::find_references()` walks all `.st` files and uses `SrcTree::find_class_references()` to collect every occurrence of the class name across `identifier`, `string`, `symbol`, and `ston_symbol` nodes — covering all three Tonel name forms (`ClassName`, `#ClassName`, `#'ClassName'`, `'ClassName'`).
3. If `include_declaration` is false, the definition location (from `class_index`) is excluded from the results.

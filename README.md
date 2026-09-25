# Tonel Smalltalk Language Server

A Language Server Protocol (LSP) implementation for [Tonel-Smalltalk](https://github.com/mumez/tree-sitter-tonel-smalltalk), targeting the [Zed](https://zed.dev) editor.

## Features

- Syntax highlighting (via Zed native tree-sitter integration)
- Go-to-definition for class names
- Find all references for class names (supports `#ClassName`, `#'ClassName'`, `'ClassName'` forms)
- Hover information for class/trait names (kind, superclass, instance/class variables) and instance/class variables (owning classes)
- Diagnostics: syntax error reporting via tree-sitter parse error detection

## Requirements

- Rust (stable)

## Installation

### Build the LSP server

```bash
cargo build --release
```

Place the binary on your PATH:

```bash
cp target/release/tonel-smalltalk-language-server ~/.local/bin/
```

### Claude Code

This repository is also a Claude Code plugin marketplace providing the `tonel-smalltalk-lsp` plugin, which connects the language server to `.st` files. The plugin does not bundle the binary, so place `tonel-smalltalk-language-server` on your PATH first (see above).

```bash
claude plugin marketplace add mumez/tonel-smalltalk-language-server
claude plugin install tonel-smalltalk-lsp@tonel-smalltalk-language-server
```

Or inside a Claude Code session:

```
/plugin marketplace add mumez/tonel-smalltalk-language-server
/plugin install tonel-smalltalk-lsp@tonel-smalltalk-language-server
```

## Development

Run the tests:

```bash
cargo test
```

## License

MIT

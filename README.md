# Tonel Smalltalk Language Server

A Language Server Protocol (LSP) implementation for [Tonel-Smalltalk](https://github.com/mumez/tree-sitter-tonel-smalltalk), targeting the [Zed](https://zed.dev) editor.

## Features

- Syntax highlighting (via Zed native tree-sitter integration)
- Go-to-definition for class names

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

## Development

Run the tests:

```bash
cargo test
```

## License

MIT

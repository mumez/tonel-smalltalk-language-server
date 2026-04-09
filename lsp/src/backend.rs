use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use dashmap::DashMap;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

use crate::src_tree::{normalize_class_name, SrcTree};
use crate::workspace::Workspace;

pub struct Backend {
    client: Client,
    workspace_root: RwLock<Option<PathBuf>>,
    document_map: DashMap<Url, SrcTree>,
    workspace: Arc<Workspace>,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            workspace_root: RwLock::new(None),
            document_map: DashMap::new(),
            workspace: Arc::new(Workspace::new()),
        }
    }

    async fn on_change(&self, uri: Url, text: String) {
        let src_tree = SrcTree::new(text);
        self.workspace.update(uri.clone(), &src_tree);
        self.document_map.insert(uri, src_tree);
    }

    async fn log(&self, level: MessageType, msg: impl Into<String>) {
        self.client.log_message(level, msg.into()).await;
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        if let Some(root_uri) = params.root_uri {
            if let Ok(path) = root_uri.to_file_path() {
                *self.workspace_root.write().unwrap() = Some(path);
            }
        }

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::FULL),
                        ..Default::default()
                    },
                )),
                definition_provider: Some(OneOf::Left(true)),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        let root = self.workspace_root.read().unwrap().clone();
        if let Some(root_path) = root {
            let workspace = Arc::clone(&self.workspace);
            let client = self.client.clone();
            tokio::spawn(async move {
                client
                    .log_message(
                        MessageType::INFO,
                        format!("Workspace scan starting: root_path={}", root_path.display()),
                    )
                    .await;
                let result =
                    tokio::task::spawn_blocking(move || workspace.scan(&root_path)).await;
                match result {
                    Ok(Ok(count)) => {
                        client
                            .log_message(
                                MessageType::INFO,
                                format!("Workspace scan complete: {} classes indexed", count),
                            )
                            .await
                    }
                    _ => {
                        client
                            .log_message(MessageType::ERROR, "Workspace scan failed")
                            .await
                    }
                }
            });
        } else {
            self.log(
                MessageType::WARNING,
                "No workspace root: scan skipped (index will be built from opened files)",
            )
            .await;
        }
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        self.on_change(params.text_document.uri, params.text_document.text)
            .await;
    }

    async fn did_change(&self, mut params: DidChangeTextDocumentParams) {
        self.on_change(
            params.text_document.uri,
            std::mem::take(&mut params.content_changes[0].text),
        )
        .await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.document_map.remove(&params.text_document.uri);
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let pos = &params.text_document_position_params;
        let uri = &pos.text_document.uri;
        let doc_count = self.document_map.len();

        // tree-sitter's Node is not Send, so all tree-sitter work must complete
        // before the first .await. The outcome enum carries only owned data.
        enum Outcome {
            NotInMap,
            NotIdentifier(String),
            NotUppercase(String),
            Lookup(String),
        }

        let outcome = {
            let src_tree = match self.document_map.get(uri) {
                Some(t) => t,
                None => {
                    // Log immediately; this early-return path can't use the helper.
                    self.client
                        .log_message(
                            MessageType::WARNING,
                            format!(
                                "goto_definition: uri={} pos={}:{} doc_map={} — document not in map",
                                uri, pos.position.line, pos.position.character, doc_count
                            ),
                        )
                        .await;
                    return Ok(None);
                }
            };

            let point = tree_sitter::Point {
                row: pos.position.line as usize,
                column: pos.position.character as usize,
            };
            let root = src_tree.tree().root_node();
            // When the cursor is just past an identifier (e.g. at ')'), try column-1 as fallback.
            let node = root.descendant_for_point_range(point, point).and_then(|n| {
                let k = n.kind();
                if k != "identifier" && k != "string" && k != "symbol" && point.column > 0 {
                    let prev = tree_sitter::Point { row: point.row, column: point.column - 1 };
                    root.descendant_for_point_range(prev, prev)
                } else {
                    Some(n)
                }
            });
            if let Some(n) = node {
                let kind = n.kind();
                if kind == "identifier" || kind == "string" || kind == "symbol" {
                    match n.utf8_text(src_tree.src().as_bytes()) {
                        Ok(raw) => {
                            let s = normalize_class_name(raw);
                            if s.starts_with(|c: char| c.is_uppercase()) {
                                Outcome::Lookup(s.to_string())
                            } else {
                                Outcome::NotUppercase(s.to_string())
                            }
                        }
                        Err(_) => Outcome::NotInMap,
                    }
                } else {
                    Outcome::NotIdentifier(kind.to_string())
                }
            } else {
                Outcome::NotInMap
            }
        };

        self.log(
            MessageType::INFO,
            format!(
                "goto_definition: uri={} pos={}:{} doc_map={}",
                uri, pos.position.line, pos.position.character, doc_count
            ),
        )
        .await;

        match outcome {
            Outcome::NotInMap => Ok(None),
            Outcome::NotIdentifier(kind) => {
                self.log(
                    MessageType::INFO,
                    format!("goto_definition: node kind='{}' (not identifier) — skip", kind),
                )
                .await;
                Ok(None)
            }
            Outcome::NotUppercase(ident) => {
                self.log(
                    MessageType::INFO,
                    format!("goto_definition: '{}' not uppercase — skip", ident),
                )
                .await;
                Ok(None)
            }
            Outcome::Lookup(ident) => {
                let class_count = self.workspace.class_count();
                self.log(
                    MessageType::INFO,
                    format!(
                        "goto_definition: looking up '{}' (indexed_classes={})",
                        ident, class_count
                    ),
                )
                .await;
                match self.workspace.find_class(&ident) {
                    Some((url, range)) => {
                        self.log(
                            MessageType::INFO,
                            format!("goto_definition: found '{}' at {}", ident, url),
                        )
                        .await;
                        Ok(Some(GotoDefinitionResponse::Scalar(Location {
                            uri: url,
                            range,
                        })))
                    }
                    None => {
                        self.log(
                            MessageType::WARNING,
                            format!("goto_definition: '{}' not found in class index", ident),
                        )
                        .await;
                        Ok(None)
                    }
                }
            }
        }
    }
}

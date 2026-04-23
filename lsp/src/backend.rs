use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use dashmap::DashMap;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

use crate::src_tree::{ClassAtPos, SrcTree};
use crate::workspace::Workspace;

fn range_contains(outer: &Range, inner: &Range) -> bool {
    let start_ok = outer.start.line < inner.start.line
        || (outer.start.line == inner.start.line
            && outer.start.character <= inner.start.character);
    let end_ok = outer.end.line > inner.end.line
        || (outer.end.line == inner.end.line && outer.end.character >= inner.end.character);
    start_ok && end_ok
}

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
        let diagnostics = src_tree.syntax_errors();
        self.document_map.insert(uri.clone(), src_tree);
        self.client.publish_diagnostics(uri, diagnostics, None).await;
    }

    async fn log(&self, level: MessageType, msg: impl Into<String>) {
        self.client.log_message(level, msg.into()).await;
    }

    /// Resolves the class name at the cursor position. Returns `(class_name, doc_count)`
    /// on success, or logs and returns `None` on failure.
    /// Must be called inside a scope block so the DashMap guard is dropped before any `.await`.
    fn resolve_class_at(&self, uri: &Url, pos: &Position) -> (usize, Option<ClassAtPos>) {
        let doc_count = self.document_map.len();
        let result = self
            .document_map
            .get(uri)
            .map(|t| t.class_at_position(pos));
        (doc_count, result)
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
                references_provider: Some(OneOf::Left(true)),
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
        let uri = params.text_document.uri;
        self.document_map.remove(&uri);
        self.client.publish_diagnostics(uri, vec![], None).await;
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let pos = &params.text_document_position;
        let uri = &pos.text_document.uri;
        let include_declaration = params.context.include_declaration;

        // tree-sitter's Node is not Send; resolve before any .await.
        let (doc_count, class_at_pos) = self.resolve_class_at(uri, &pos.position);

        self.log(
            MessageType::INFO,
            format!(
                "references: uri={} pos={}:{} doc_map={} include_declaration={}",
                uri, pos.position.line, pos.position.character, doc_count, include_declaration
            ),
        )
        .await;

        let class_name = match class_at_pos {
            None => {
                self.log(
                    MessageType::WARNING,
                    format!(
                        "references: uri={} pos={}:{} doc_map={} — document not in map",
                        uri, pos.position.line, pos.position.character, doc_count
                    ),
                )
                .await;
                return Ok(None);
            }
            Some(ClassAtPos::NotNode) => return Ok(None),
            Some(ClassAtPos::NotIdentifier(kind)) => {
                self.log(
                    MessageType::INFO,
                    format!("references: node kind='{}' (not identifier) — skip", kind),
                )
                .await;
                return Ok(None);
            }
            Some(ClassAtPos::NotUppercase(ident)) => {
                self.log(
                    MessageType::INFO,
                    format!("references: '{}' not uppercase — skip", ident),
                )
                .await;
                return Ok(None);
            }
            Some(ClassAtPos::Found(name)) => name,
        };

        self.log(
            MessageType::INFO,
            format!(
                "references: looking up '{}' (indexed_classes={})",
                class_name,
                self.workspace.class_count()
            ),
        )
        .await;

        let mut result = self.workspace.find_references(&class_name);

        if !include_declaration {
            if let Some((def_url, def_range)) = self.workspace.find_class(&class_name) {
                result.retain(|loc| !(loc.uri == def_url && range_contains(&def_range, &loc.range)));
            }
        }

        if result.is_empty() {
            self.log(
                MessageType::WARNING,
                format!("references: '{}' — no references found", class_name),
            )
            .await;
        } else {
            self.log(
                MessageType::INFO,
                format!(
                    "references: '{}' found {} locations (include_declaration={})",
                    class_name,
                    result.len(),
                    include_declaration
                ),
            )
            .await;
        }

        Ok(Some(result))
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let pos = &params.text_document_position_params;
        let uri = &pos.text_document.uri;

        // tree-sitter's Node is not Send; resolve before any .await.
        let (doc_count, class_at_pos) = self.resolve_class_at(uri, &pos.position);

        self.log(
            MessageType::INFO,
            format!(
                "goto_definition: uri={} pos={}:{} doc_map={}",
                uri, pos.position.line, pos.position.character, doc_count
            ),
        )
        .await;

        let class_name = match class_at_pos {
            None => {
                self.log(
                    MessageType::WARNING,
                    format!(
                        "goto_definition: uri={} pos={}:{} doc_map={} — document not in map",
                        uri, pos.position.line, pos.position.character, doc_count
                    ),
                )
                .await;
                return Ok(None);
            }
            Some(ClassAtPos::NotNode) => return Ok(None),
            Some(ClassAtPos::NotIdentifier(kind)) => {
                self.log(
                    MessageType::INFO,
                    format!("goto_definition: node kind='{}' (not identifier) — skip", kind),
                )
                .await;
                return Ok(None);
            }
            Some(ClassAtPos::NotUppercase(ident)) => {
                self.log(
                    MessageType::INFO,
                    format!("goto_definition: '{}' not uppercase — skip", ident),
                )
                .await;
                return Ok(None);
            }
            Some(ClassAtPos::Found(name)) => name,
        };

        self.log(
            MessageType::INFO,
            format!(
                "goto_definition: looking up '{}' (indexed_classes={})",
                class_name,
                self.workspace.class_count()
            ),
        )
        .await;

        match self.workspace.find_class(&class_name) {
            Some((url, range)) => {
                self.log(
                    MessageType::INFO,
                    format!("goto_definition: found '{}' at {}", class_name, url),
                )
                .await;
                Ok(Some(GotoDefinitionResponse::Scalar(Location { uri: url, range })))
            }
            None => {
                self.log(
                    MessageType::WARNING,
                    format!("goto_definition: '{}' not found in class index", class_name),
                )
                .await;
                Ok(None)
            }
        }
    }
}

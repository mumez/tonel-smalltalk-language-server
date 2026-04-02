use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use dashmap::DashMap;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

use crate::src_tree::SrcTree;
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
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
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
                let result =
                    tokio::task::spawn_blocking(move || workspace.scan(&root_path)).await;
                match result {
                    Ok(Ok(())) => {
                        client
                            .log_message(MessageType::INFO, "Workspace scan complete")
                            .await
                    }
                    _ => {
                        client
                            .log_message(MessageType::ERROR, "Workspace scan failed")
                            .await
                    }
                }
            });
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

        let src_tree = match self.document_map.get(&pos.text_document.uri) {
            Some(t) => t,
            None => return Ok(None),
        };

        let point = tree_sitter::Point {
            row: pos.position.line as usize,
            column: pos.position.character as usize,
        };

        let root = src_tree.tree().root_node();
        let node = match root.descendant_for_point_range(point, point) {
            Some(n) => n,
            None => return Ok(None),
        };

        if node.kind() != "identifier" {
            return Ok(None);
        }

        let ident = match node.utf8_text(src_tree.src().as_bytes()) {
            Ok(s) => s,
            Err(_) => return Ok(None),
        };

        // Only class names (uppercase first character) trigger go-to-definition.
        if !ident.starts_with(|c: char| c.is_uppercase()) {
            return Ok(None);
        }

        let ident = ident.to_string();
        drop(src_tree);

        match self.workspace.find_class(&ident) {
            Some((url, range)) => Ok(Some(GotoDefinitionResponse::Scalar(Location {
                uri: url,
                range,
            }))),
            None => Ok(None),
        }
    }
}

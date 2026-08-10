//! Thin tower-lsp wire layer over the query helpers in the crate root.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use tower_lsp::jsonrpc::Result as LspResult;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

use cmakedb_db::Db;

struct State {
    db_path: PathBuf,
    /// open document text, by absolute editor path
    open_docs: HashMap<String, String>,
}

pub struct Backend {
    client: Client,
    state: Mutex<Option<State>>,
}

impl Backend {
    fn with_db<T>(&self, f: impl FnOnce(&Db, &State) -> T) -> Option<T> {
        let guard = self.state.lock().ok()?;
        let state = guard.as_ref()?;
        // SQLite connections are cheap to open and this keeps the server
        // free of long-lived connection state across edits.
        let db = Db::open(&state.db_path).ok()?;
        Some(f(&db, state))
    }

    fn editor_path(uri: &Url) -> Option<String> {
        uri.to_file_path()
            .ok()
            .map(|p| p.to_string_lossy().to_string())
    }

    async fn publish_diagnostics_for(&self, uri: Url) {
        let Some(path) = Self::editor_path(&uri) else {
            return;
        };
        let diags = self.with_db(|db, state| {
            let db_path = crate::db_path_for(db, std::path::Path::new(&path));
            let all = crate::diagnostics(db).unwrap_or_default();
            let stale = state
                .open_docs
                .get(&path)
                .map(|text| crate::is_stale(db, &db_path, text))
                .unwrap_or(false);
            let mut out = Vec::new();
            for d in all.get(&db_path).cloned().unwrap_or_default() {
                let prefix = if stale { "[stale recording] " } else { "" };
                let line = (d.line.max(1) - 1) as u32;
                out.push(Diagnostic {
                    range: Range::new(Position::new(line, 0), Position::new(line, u32::MAX)),
                    severity: Some(match d.severity {
                        cmakedb_passes::Severity::Error => DiagnosticSeverity::ERROR,
                        cmakedb_passes::Severity::Warning => DiagnosticSeverity::WARNING,
                        cmakedb_passes::Severity::Note => DiagnosticSeverity::INFORMATION,
                    }),
                    code: Some(NumberOrString::String(d.rule)),
                    source: Some("cmakedb".into()),
                    message: format!("{prefix}{}", d.message),
                    ..Default::default()
                });
            }
            out
        });
        if let Some(diags) = diags {
            self.client.publish_diagnostics(uri, diags, None).await;
        }
    }

    fn position_word(&self, uri: &Url, pos: Position) -> Option<(String, String, i64, String)> {
        let path = Self::editor_path(uri)?;
        self.with_db(|db, state| {
            let text = state.open_docs.get(&path)?.clone();
            let word = crate::word_at(&text, pos.line as usize, pos.character as usize)?;
            let db_path = crate::db_path_for(db, std::path::Path::new(&path));
            Some((path.clone(), db_path, pos.line as i64 + 1, word))
        })?
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> LspResult<InitializeResult> {
        let root = params
            .root_uri
            .and_then(|u| u.to_file_path().ok())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        if let Some(db_path) = crate::find_db(&root) {
            *self.state.lock().unwrap() = Some(State {
                db_path,
                open_docs: HashMap::new(),
            });
        }
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "cmakedb".into(),
                version: Some(env!("CARGO_PKG_VERSION").into()),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        let have_db = self.state.lock().map(|s| s.is_some()).unwrap_or(false);
        let msg = if have_db {
            "cmakedb: recording loaded; diagnostics are post-mortem (re-run `cmakedb record` \
             after CMake changes)"
        } else {
            "cmakedb: no .cmakedb/trace.db found under the workspace — run `cmakedb record` \
             first"
        };
        self.client.log_message(MessageType::INFO, msg).await;
    }

    async fn shutdown(&self) -> LspResult<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        if let Some(path) = Self::editor_path(&uri) {
            if let Ok(mut guard) = self.state.lock() {
                if let Some(state) = guard.as_mut() {
                    state.open_docs.insert(path, params.text_document.text);
                }
            }
        }
        self.publish_diagnostics_for(uri).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        if let (Some(path), Some(change)) = (
            Self::editor_path(&uri),
            params.content_changes.into_iter().last(),
        ) {
            if let Ok(mut guard) = self.state.lock() {
                if let Some(state) = guard.as_mut() {
                    state.open_docs.insert(path, change.text);
                }
            }
        }
        // Recompute so the stale marker reacts to edits.
        self.publish_diagnostics_for(uri).await;
    }

    async fn hover(&self, params: HoverParams) -> LspResult<Option<Hover>> {
        let pos = params.text_document_position_params;
        let Some((_path, db_path, line, word)) =
            self.position_word(&pos.text_document.uri, pos.position)
        else {
            return Ok(None);
        };
        let content = self
            .with_db(|db, _| crate::hover(db, &db_path, line, &word).ok().flatten())
            .flatten();
        Ok(content.map(|text| Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: text,
            }),
            range: None,
        }))
    }

    async fn code_action(&self, params: CodeActionParams) -> LspResult<Option<CodeActionResponse>> {
        let uri = params.text_document.uri;
        let Some(path) = Self::editor_path(&uri) else {
            return Ok(None);
        };
        let line = params.range.start.line as i64 + 1;
        let actions = self.with_db(|db, state| {
            let db_path = crate::db_path_for(db, std::path::Path::new(&path));
            // Map open buffers to recording paths for the staleness guard.
            let mut current: std::collections::HashMap<String, String> =
                std::collections::HashMap::new();
            for (p, text) in &state.open_docs {
                current.insert(
                    crate::db_path_for(db, std::path::Path::new(p)),
                    text.clone(),
                );
            }
            crate::code_actions(db, &db_path, line, &current).unwrap_or_default()
        });
        let Some(actions) = actions else {
            return Ok(None);
        };
        let response: Vec<CodeActionOrCommand> = actions
            .into_iter()
            .filter_map(|a| {
                let mut changes: std::collections::HashMap<Url, Vec<TextEdit>> =
                    std::collections::HashMap::new();
                for (file, sl, sc, el, ec, text) in a.edits {
                    let uri = Url::from_file_path(&file).ok()?;
                    changes.entry(uri).or_default().push(TextEdit {
                        range: Range::new(Position::new(sl, sc), Position::new(el, ec)),
                        new_text: text,
                    });
                }
                Some(CodeActionOrCommand::CodeAction(CodeAction {
                    title: a.title,
                    kind: Some(CodeActionKind::QUICKFIX),
                    edit: Some(WorkspaceEdit {
                        changes: Some(changes),
                        ..Default::default()
                    }),
                    ..Default::default()
                }))
            })
            .collect();
        Ok(if response.is_empty() {
            None
        } else {
            Some(response)
        })
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> LspResult<Option<GotoDefinitionResponse>> {
        let pos = params.text_document_position_params;
        let Some((_path, db_path, line, word)) =
            self.position_word(&pos.text_document.uri, pos.position)
        else {
            return Ok(None);
        };
        let hit = self
            .with_db(|db, _| crate::definition(db, &db_path, line, &word).ok().flatten())
            .flatten();
        Ok(hit.and_then(|(file, line)| {
            let uri = Url::from_file_path(&file).ok()?;
            Some(GotoDefinitionResponse::Scalar(Location {
                uri,
                range: Range::new(
                    Position::new((line.max(1) - 1) as u32, 0),
                    Position::new((line.max(1) - 1) as u32, 0),
                ),
            }))
        }))
    }
}

// --- Custom requests (§2.4): power the editor provenance tree view --------

#[derive(serde::Deserialize)]
struct ProvenanceParams {
    target: String,
    dep: String,
    /// "link" (default) | "include" | "define" | "option"
    #[serde(default)]
    kind: Option<String>,
}

#[derive(serde::Deserialize)]
struct EdgesParams {
    target: String,
}

impl Backend {
    async fn provenance_req(
        &self,
        params: ProvenanceParams,
    ) -> LspResult<Option<serde_json::Value>> {
        Ok(self
            .with_db(|db, _| {
                let ex = match params.kind.as_deref() {
                    None | Some("link") => {
                        cmakedb_passes::provenance::why_links(db, &params.target, &params.dep)
                    }
                    Some(kind) => cmakedb_passes::provenance::why_requirement(
                        db,
                        &params.target,
                        kind,
                        &params.dep,
                    ),
                };
                ex.ok().and_then(|e| serde_json::to_value(e).ok())
            })
            .flatten())
    }

    async fn targets_req(&self, _params: serde_json::Value) -> LspResult<serde_json::Value> {
        let rows = self
            .with_db(|db, _| crate::targets_list(db).unwrap_or_default())
            .unwrap_or_default();
        Ok(serde_json::json!(rows
            .into_iter()
            .map(|(name, ttype)| serde_json::json!({"name": name, "type": ttype}))
            .collect::<Vec<_>>()))
    }

    async fn edges_req(&self, params: EdgesParams) -> LspResult<serde_json::Value> {
        let rows = self
            .with_db(|db, _| crate::edges_of(db, &params.target).unwrap_or_default())
            .unwrap_or_default();
        Ok(serde_json::json!(rows
            .into_iter()
            .map(|(dst, vis, origin, is_target)| serde_json::json!({
                "dst": dst, "visibility": vis, "origin": origin, "isTarget": is_target
            }))
            .collect::<Vec<_>>()))
    }
}

/// Run the language server over stdio (blocks until the client disconnects).
pub fn run_stdio() -> anyhow::Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let stdin = tokio::io::stdin();
        let stdout = tokio::io::stdout();
        let (service, socket) = LspService::build(|client| Backend {
            client,
            state: Mutex::new(None),
        })
        .custom_method("cmakedb/provenance", Backend::provenance_req)
        .custom_method("cmakedb/targets", Backend::targets_req)
        .custom_method("cmakedb/edges", Backend::edges_req)
        .finish();
        Server::new(stdin, stdout, socket).serve(service).await;
    });
    Ok(())
}

use std::{collections::HashMap, time::Duration};

use tower_lsp::{
    jsonrpc,
    lsp_types::{
        CodeAction, CodeActionKind, CodeActionOptions, CodeActionOrCommand, CodeActionParams,
        CodeActionProviderCapability, CodeActionResponse, DidChangeTextDocumentParams,
        DidCloseTextDocumentParams, DidOpenTextDocumentParams, DidSaveTextDocumentParams,
        InitializeParams, InitializeResult, InitializedParams, MessageType, PositionEncodingKind,
        PublishDiagnosticsClientCapabilities, ServerCapabilities, ServerInfo,
        TextDocumentClientCapabilities, TextDocumentSyncCapability, TextDocumentSyncKind, TextEdit,
        WorkDoneProgressOptions, WorkspaceEdit,
    },
    LanguageServer,
};

use crate::{bacon::validate_bacon_preferences, BaconLs, DiagnosticData, PKG_NAME, PKG_VERSION};

#[tower_lsp::async_trait]
impl LanguageServer for BaconLs {
    async fn initialize(&self, params: InitializeParams) -> jsonrpc::Result<InitializeResult> {
        tracing::info!("initializing {PKG_NAME} v{PKG_VERSION}",);
        tracing::debug!("initializing with input parameters: {params:#?}");

        if let Some(TextDocumentClientCapabilities {
            publish_diagnostics:
                Some(PublishDiagnosticsClientCapabilities {
                    data_support: Some(true),
                    ..
                }),
            ..
        }) = params.capabilities.text_document
        {
            tracing::info!("client supports diagnostics data and diagnostics")
        } else {
            tracing::error!("client does not support diagnostics data");
            return Err(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidRequest));
        }

        let mut state = self.state.write().await;
        state.workspace_folders = params.workspace_folders;

        if let Some(ops) = params.initialization_options {
            if let Some(values) = ops.as_object() {
                tracing::debug!("client initialization options: {:#?}", values);
                if let Some(value) = values.get("locationsFile") {
                    state.locations_file = value
                        .as_str()
                        .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidParams))?
                        .to_string();
                }
                if let Some(value) = values.get("updateOnSave") {
                    state.update_on_save = value
                        .as_bool()
                        .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidParams))?;
                }
                if let Some(value) = values.get("updateOnSaveWaitMillis") {
                    state.update_on_save_wait_millis = Duration::from_millis(
                        value
                            .as_u64()
                            .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidParams))?,
                    );
                }
                if let Some(value) = values.get("updateOnChange") {
                    state.update_on_change = value
                        .as_bool()
                        .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidParams))?;
                }
                if let Some(value) = values.get("validateBaconPreferences") {
                    state.validate_bacon_preferences = value
                        .as_bool()
                        .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidParams))?;
                }
            }
        }
        tracing::debug!("loaded state from lsp settings: {state:#?}");
        drop(state);

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                // Only support UTF-16 positions for now, which is the default when unspecified
                position_encoding: Some(PositionEncodingKind::UTF16),
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                code_action_provider: Some(CodeActionProviderCapability::Options(
                    CodeActionOptions {
                        code_action_kinds: Some(vec![CodeActionKind::QUICKFIX]),
                        work_done_progress_options: WorkDoneProgressOptions {
                            work_done_progress: Some(false),
                        },
                        resolve_provider: None,
                    },
                )),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: PKG_NAME.to_string(),
                version: Some(PKG_VERSION.to_string()),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        if let Some(client) = self.client.as_ref() {
            tracing::info!("{PKG_NAME} v{PKG_VERSION} lsp server initialized");
            client
                .log_message(
                    MessageType::INFO,
                    format!("{PKG_NAME} v{PKG_VERSION} lsp server initialized"),
                )
                .await;
            let guard = self.state.read().await;
            let validate = guard.validate_bacon_preferences;
            drop(guard);
            if validate {
                if let Err(e) = validate_bacon_preferences().await {
                    tracing::error!("{e}");
                    client.show_message(MessageType::ERROR, e).await;
                }
            } else {
                tracing::warn!(
                    "skipping validation of bacon preferences, validateBaconPreferences is false"
                );
            }
        }
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        tracing::debug!("client sent didOpen request");
        self.publish_diagnostics(&params.text_document.uri).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        tracing::debug!("client sent didClose request");
        self.publish_diagnostics(&params.text_document.uri).await;
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let guard = self.state.read().await;
        let update_on_save = guard.update_on_save;
        let update_on_save_wait_millis = guard.update_on_save_wait_millis;
        drop(guard);
        tracing::debug!("client sent didSave request, updateOnSave is {update_on_save} after waiting bacon for {update_on_save_wait_millis:?}");
        if update_on_save {
            tokio::time::sleep(update_on_save_wait_millis).await;
            self.publish_diagnostics(&params.text_document.uri).await;
        }
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let update_on_change = self.state.read().await.update_on_change;
        tracing::debug!("client sent didChange request, updateOnChange is {update_on_change}");
        if update_on_change {
            self.publish_diagnostics(&params.text_document.uri).await;
        }
    }

    async fn code_action(
        &self,
        params: CodeActionParams,
    ) -> jsonrpc::Result<Option<CodeActionResponse>> {
        tracing::debug!("code_action: {params:?}");

        let actions = params
            .context
            .diagnostics
            .iter()
            .filter(|diag| diag.source == Some("bacon-ls".to_string()))
            .flat_map(|diag| match &diag.data {
                Some(data) => {
                    if let Ok(DiagnosticData { corrections }) =
                        serde_json::from_value::<DiagnosticData>(data.clone())
                    {
                        corrections
                            .iter()
                            .map(|c| {
                                CodeActionOrCommand::CodeAction(CodeAction {
                                    title: "Replace with clippy suggestion".to_string(),
                                    kind: Some(CodeActionKind::QUICKFIX),
                                    diagnostics: Some(vec![diag.clone()]),
                                    edit: Some(WorkspaceEdit {
                                        changes: Some(HashMap::from([(
                                            params.text_document.uri.clone(),
                                            vec![TextEdit {
                                                range: diag.range,
                                                new_text: c.to_string(),
                                            }],
                                        )])),
                                        ..WorkspaceEdit::default()
                                    }),
                                    is_preferred: if corrections.len() == 1 {
                                        Some(true)
                                    } else {
                                        None
                                    },
                                    ..CodeAction::default()
                                })
                            })
                            .collect()
                    } else {
                        tracing::error!(
                            "deserialization failed: received {data:?} as diagnostic data",
                        );
                        vec![]
                    }
                }
                None => {
                    tracing::warn!("client doesn't support diagnostic data");
                    vec![]
                }
            })
            .collect::<Vec<_>>();

        Ok(Some(actions))
    }

    async fn shutdown(&self) -> jsonrpc::Result<()> {
        if let Some(client) = self.client.as_ref() {
            tracing::info!("{PKG_NAME} v{PKG_VERSION} lsp server stopped");
            client
                .log_message(
                    MessageType::INFO,
                    format!("{PKG_NAME} v{PKG_VERSION} lsp server stopped"),
                )
                .await;
        }
        Ok(())
    }
}

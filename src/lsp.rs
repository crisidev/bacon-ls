use std::collections::HashMap;

use ls_types::{
    CodeAction, CodeActionKind, CodeActionOptions, CodeActionOrCommand, CodeActionParams, CodeActionProviderCapability,
    CodeActionResponse, DeleteFilesParams, DidChangeConfigurationParams, DidChangeTextDocumentParams,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, DidSaveTextDocumentParams, InitializeParams,
    InitializeResult, InitializedParams, MessageType, PositionEncodingKind, PublishDiagnosticsClientCapabilities,
    RenameFilesParams, ServerCapabilities, ServerInfo, TextDocumentClientCapabilities, TextDocumentSyncCapability,
    TextDocumentSyncKind, TextEdit, Uri, WorkDoneProgressOptions, WorkspaceEdit,
};
use tower_lsp_server::{LanguageServer, jsonrpc};

use crate::{BackendChoice, BackendRuntime, BaconLs, Cargo, DiagnosticData, PKG_NAME, PKG_VERSION};

impl LanguageServer for BaconLs {
    async fn initialize(&self, params: InitializeParams) -> jsonrpc::Result<InitializeResult> {
        tracing::info!("initializing {PKG_NAME} v{PKG_VERSION}",);
        tracing::debug!("initializing with input parameters: {params:#?}");
        let project_root = Cargo::find_project_root(&params).await;
        tracing::debug!("Found project root: {project_root:?}");

        if let Some(TextDocumentClientCapabilities {
            publish_diagnostics: Some(PublishDiagnosticsClientCapabilities { .. }),
            ..
        }) = params.capabilities.text_document
        {
            tracing::info!("client supports diagnostics");
        } else {
            tracing::warn!("client does not support diagnostics");
            return Err(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidRequest));
        }

        let mut diagnostics_data_supported = false;
        if let Some(TextDocumentClientCapabilities {
            publish_diagnostics:
                Some(PublishDiagnosticsClientCapabilities {
                    data_support: Some(true),
                    ..
                }),
            ..
        }) = params.capabilities.text_document
        {
            tracing::info!("client supports diagnostics data");
            diagnostics_data_supported = true;
        } else {
            tracing::warn!("client does not support diagnostics data");
        }

        let mut state = self.state.write().await;
        state.project_root = project_root;
        state.workspace_folders = params.workspace_folders;
        state.diagnostics_data_supported = diagnostics_data_supported;
        tracing::trace!("loaded state from lsp settings: {state:#?}");
        drop(state);

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                // Only support UTF-16 positions for now, which is the default when unspecified
                position_encoding: Some(PositionEncodingKind::UTF16),
                text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
                code_action_provider: Some(CodeActionProviderCapability::Options(CodeActionOptions {
                    code_action_kinds: Some(vec![CodeActionKind::QUICKFIX]),
                    work_done_progress_options: WorkDoneProgressOptions {
                        work_done_progress: Some(false),
                    },
                    resolve_provider: None,
                })),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: PKG_NAME.to_string(),
                version: Some(PKG_VERSION.to_string()),
            }),
            // See <https://clangd.llvm.org/extensions.html#utf-8-offsets>.
            // which says:
            // ```
            // This extension has been deprecated with clangd-21 in favor of
            // the positionEncoding introduced in LSP 3.17. It’ll go away with clangd-23
            // ```
            // So None should be fine
            offset_encoding: None,
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.pull_configuration().await;

        let state = self.state.read().await;
        let Some(runtime) = state.backend.as_ref() else {
            tracing::error!("No backend initialized");
            return;
        };
        let backend_chosen = runtime.backend_choice();
        drop(state);

        tracing::info!("{PKG_NAME} v{PKG_VERSION} lsp server initialized with backend: {backend_chosen:?}");
        self.client
            .log_message(
                MessageType::INFO,
                format!("{PKG_NAME} v{PKG_VERSION} lsp server initialized with backend: {backend_chosen:?}"),
            )
            .await;

        tracing::info!("initialized complete");

        if backend_chosen == BackendChoice::Cargo {
            self.publish_cargo_diagnostics().await
        }
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        tracing::info!("client sent didChangeConfiguration");
        if let Some(settings) = params.settings.as_object()
            && !settings.is_empty()
        {
            tracing::info!("using client provided settings");
            self.adapt_to_settings(params.settings).await;
        } else {
            tracing::info!("settings is either not an object or is empty");
            self.pull_configuration().await;
        }
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        tracing::trace!("client sent didOpen request");
        let mut state = self.state.write().await;
        if let Some(BackendRuntime::Bacon { runtime, .. }) = &mut state.backend {
            runtime.open_files.insert(params.text_document.uri.clone());
            drop(state);
            self.publish_bacon_diagnostics(&params.text_document.uri).await;
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        tracing::trace!("client sent didClose request");
        let mut state = self.state.write().await;
        if let Some(BackendRuntime::Bacon { runtime, .. }) = &mut state.backend {
            runtime.open_files.remove(&params.text_document.uri);
            drop(state);
            self.publish_bacon_diagnostics(&params.text_document.uri).await;
        }
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        tracing::debug!("client sent didSave request");
        let state = self.state.read().await;
        let Some(backend) = &state.backend else {
            return;
        };
        match backend {
            BackendRuntime::Bacon { config, .. } => {
                if config.update_on_save {
                    if !config.update_on_save_wait.is_zero() {
                        tokio::time::sleep(config.update_on_save_wait).await;
                    }
                    drop(state);
                    self.publish_bacon_diagnostics(&params.text_document.uri).await;
                }
            }
            BackendRuntime::Cargo { .. } => {
                drop(state);
                self.publish_cargo_diagnostics().await;
            }
        }
    }

    async fn did_change(&self, _params: DidChangeTextDocumentParams) {
        tracing::trace!("client sent didChange request, nothing to do");
    }

    async fn did_delete_files(&self, params: DeleteFilesParams) {
        tracing::debug!("client sent didDeleteFiles request for {:?}", params.files);
        let mut state = self.state.write().await;
        if let Some(BackendRuntime::Bacon { runtime, .. }) = &mut state.backend {
            for file in params.files {
                if let Ok(uri) = str::parse::<Uri>(&file.uri) {
                    runtime.open_files.remove(&uri);
                }
            }
        }
        drop(state);
    }

    async fn did_rename_files(&self, params: RenameFilesParams) {
        tracing::debug!("client sent didRenameFiles request for {:?}", params.files);
        for file in params.files {
            tracing::debug!(
                "client sent didRenameFiles request {} -> {}",
                file.old_uri,
                file.new_uri
            );
            if let (Ok(old_uri), Ok(new_uri)) = (str::parse::<Uri>(&file.old_uri), str::parse::<Uri>(&file.new_uri)) {
                let mut state = self.state.write().await;
                if let Some(BackendRuntime::Bacon { runtime, .. }) = &mut state.backend {
                    runtime.open_files.remove(&old_uri);
                    runtime.open_files.insert(new_uri.clone());
                }
                drop(state);
                self.publish_bacon_diagnostics(&new_uri).await;
            }
        }
    }

    async fn code_action(&self, params: CodeActionParams) -> jsonrpc::Result<Option<CodeActionResponse>> {
        tracing::trace!("client sent codeActions request");
        let state = self.state.read().await;
        let diagnostics_data_supported = state.diagnostics_data_supported;
        drop(state);

        if diagnostics_data_supported {
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
                                        title: "Replace with bacon-ls suggestion".to_string(),
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
                                        is_preferred: if corrections.len() == 1 { Some(true) } else { None },
                                        ..CodeAction::default()
                                    })
                                })
                                .collect()
                        } else {
                            tracing::error!("deserialization failed: received {data:?} as diagnostic data",);
                            vec![]
                        }
                    }
                    None => {
                        tracing::debug!("client doesn't support diagnostic data");
                        vec![]
                    }
                })
                .collect::<Vec<_>>();

            Ok(Some(actions))
        } else {
            Ok(None)
        }
    }

    async fn shutdown(&self) -> jsonrpc::Result<()> {
        let mut state = self.state.write().await;
        let backend = state.backend.take();
        drop(state);

        if let Some(backend) = backend {
            match backend {
                BackendRuntime::Bacon { mut runtime, .. } => {
                    runtime.shutdown_token.cancel();
                    if let Some(handle) = runtime.command_handle.take() {
                        tracing::info!("terminating bacon from running in background");
                        if let Err(e) = handle.await {
                            tracing::warn!("bacon command task failed during shutdown: {e}");
                        }
                    }
                    if let Err(e) = runtime.sync_files_handle.await {
                        tracing::warn!("sync files task failed during shutdown: {e}");
                    }
                }
                BackendRuntime::Cargo { runtime, .. } => {
                    runtime.cancel_token.cancel();
                }
            }
        }

        tracing::info!("{PKG_NAME} v{PKG_VERSION} lsp server stopped");
        self.client
            .log_message(
                MessageType::INFO,
                format!("{PKG_NAME} v{PKG_VERSION} lsp server stopped"),
            )
            .await;
        Ok(())
    }
}

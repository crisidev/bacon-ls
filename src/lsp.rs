use std::{collections::HashMap, time::Duration};

use tower_lsp::{
    jsonrpc,
    lsp_types::{
        CodeAction, CodeActionKind, CodeActionOptions, CodeActionOrCommand, CodeActionParams,
        CodeActionProviderCapability, CodeActionResponse, DeleteFilesParams,
        DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
        DidSaveTextDocumentParams, InitializeParams, InitializeResult, InitializedParams,
        MessageType, PositionEncodingKind, PublishDiagnosticsClientCapabilities, RenameFilesParams,
        ServerCapabilities, ServerInfo, TextDocumentClientCapabilities, TextDocumentSyncCapability,
        TextDocumentSyncKind, TextEdit, Url, WorkDoneProgressOptions, WorkspaceEdit,
    },
    LanguageServer,
};

use crate::{bacon::Bacon, BaconLs, DiagnosticData, PKG_NAME, PKG_VERSION};

#[tower_lsp::async_trait]
impl LanguageServer for BaconLs {
    async fn initialize(&self, params: InitializeParams) -> jsonrpc::Result<InitializeResult> {
        tracing::info!("initializing {PKG_NAME} v{PKG_VERSION}",);
        tracing::debug!("initializing with input parameters: {params:#?}");

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
        state.workspace_folders = params.workspace_folders;
        state.diagnostics_data_supported = diagnostics_data_supported;

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
                if let Some(value) = values.get("runBaconInBackground") {
                    state.run_bacon_in_background = value
                        .as_bool()
                        .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidParams))?;
                }
                if let Some(value) = values.get("runBaconInBackgroundCommandArguments") {
                    state.run_bacon_in_background_command_args = value
                        .as_str()
                        .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidParams))?
                        .to_string();
                }
                if let Some(value) = values.get("createBaconPreferencesFile") {
                    state.create_bacon_preferences_file = value
                        .as_bool()
                        .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidParams))?;
                }
                if let Some(value) = values.get("synchronizeAllOpenFilesWaitMillis") {
                    state.syncronize_all_open_files_wait_millis = Duration::from_millis(
                        value
                            .as_u64()
                            .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidParams))?,
                    );
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
        let state = self.state.read().await;
        let run_bacon = state.run_bacon_in_background;
        let bacon_command_args = state.run_bacon_in_background_command_args.clone();
        let create_bacon_prefs = state.create_bacon_preferences_file;
        let validate_prefs = state.validate_bacon_preferences;
        drop(state);

        if let Some(client) = self.client.as_ref() {
            tracing::info!("{PKG_NAME} v{PKG_VERSION} lsp server initialized");
            client
                .log_message(
                    MessageType::INFO,
                    format!("{PKG_NAME} v{PKG_VERSION} lsp server initialized"),
                )
                .await;
            if validate_prefs {
                if let Err(e) = Bacon::validate_preferences(create_bacon_prefs).await {
                    tracing::error!("{e}");
                    client.show_message(MessageType::ERROR, e).await;
                }
            } else {
                tracing::warn!(
                    "skipping validation of bacon preferences, validateBaconPreferences is false"
                );
            }

            if run_bacon {
                match Bacon::run_in_background("bacon", &bacon_command_args).await {
                    Ok(command) => {
                        tracing::info!(
                            "bacon was started successfully and is running in the background"
                        );
                        let mut state = self.state.write().await;
                        state.bacon_command_handle = Some(command);
                        drop(state);
                    }
                    Err(e) => {
                        tracing::error!("{e}");
                        client.show_message(MessageType::ERROR, e).await;
                    }
                }
            } else {
                tracing::warn!("skipping background bacon startup, runBaconInBackground is false");
            }
        } else {
            tracing::error!(
                "client doesn't seem to be connected, the LSP server will not function properly"
            );
        }
        let task_state = self.state.clone();
        let task_client = self.client.clone();
        tokio::task::spawn(Self::syncronize_diagnostics_for_all_open_files(
            task_state,
            task_client,
        ));
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        tracing::debug!("client sent didOpen request");
        let mut state = self.state.write().await;
        state.open_files.insert(params.text_document.uri.clone());
        let locations_file = state.locations_file.clone();
        let workspace_folders = state.workspace_folders.clone();
        drop(state);
        let client = self.client.clone();
        Self::publish_diagnostics(
            client.as_ref(),
            &params.text_document.uri,
            &locations_file,
            workspace_folders.as_deref(),
        )
        .await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        tracing::debug!("client sent didClose request");
        let mut state = self.state.write().await;
        state.open_files.remove(&params.text_document.uri);
        let locations_file = state.locations_file.clone();
        let workspace_folders = state.workspace_folders.clone();
        drop(state);
        let client = self.client.clone();
        Self::publish_diagnostics(
            client.as_ref(),
            &params.text_document.uri,
            &locations_file,
            workspace_folders.as_deref(),
        )
        .await;
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let state = self.state.read().await;
        let update_on_save = state.update_on_save;
        let update_on_save_wait_millis = state.update_on_save_wait_millis;
        let locations_file = state.locations_file.clone();
        let workspace_folders = state.workspace_folders.clone();
        drop(state);
        tracing::debug!("client sent didSave request, updateOnSave is {update_on_save} after waiting bacon for {update_on_save_wait_millis:?}");
        if update_on_save {
            let client = self.client.clone();
            tokio::time::sleep(update_on_save_wait_millis).await;
            Self::publish_diagnostics(
                client.as_ref(),
                &params.text_document.uri,
                &locations_file,
                workspace_folders.as_deref(),
            )
            .await;
        }
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let state = self.state.read().await;
        let update_on_change = self.state.read().await.update_on_change;
        let locations_file = state.locations_file.clone();
        let workspace_folders = state.workspace_folders.clone();
        drop(state);
        tracing::debug!("client sent didChange request, updateOnChange is {update_on_change}");
        if update_on_change {
            let client = self.client.clone();
            Self::publish_diagnostics(
                client.as_ref(),
                &params.text_document.uri,
                &locations_file,
                workspace_folders.as_deref(),
            )
            .await;
        }
    }

    async fn did_delete_files(&self, params: DeleteFilesParams) {
        tracing::debug!("client sent didDeleteFiles request");
        for file in params.files {
            if let Ok(uri) = Url::parse(&file.uri) {
                let mut state = self.state.write().await;
                state.open_files.remove(&uri);
                drop(state);
            }
        }
    }

    async fn did_rename_files(&self, params: RenameFilesParams) {
        tracing::debug!("client sent didRenameFiles request");
        for file in params.files {
            if let (Ok(old_uri), Ok(new_uri)) =
                (Url::parse(&file.old_uri), Url::parse(&file.new_uri))
            {
                let mut state = self.state.write().await;
                let locations_file = state.locations_file.clone();
                let workspace_folders = state.workspace_folders.clone();
                state.open_files.remove(&old_uri);
                state.open_files.insert(new_uri.clone());
                drop(state);
                Self::publish_diagnostics(
                    self.client.as_ref(),
                    &new_uri,
                    &locations_file,
                    workspace_folders.as_deref(),
                )
                .await;
            }
        }
    }

    async fn code_action(
        &self,
        params: CodeActionParams,
    ) -> jsonrpc::Result<Option<CodeActionResponse>> {
        tracing::debug!("code_action: {params:?}");
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
        } else {
            Ok(None)
        }
    }

    async fn shutdown(&self) -> jsonrpc::Result<()> {
        let state = self.state.read().await;
        if let Some(handle) = state.bacon_command_handle.as_ref() {
            tracing::info!("terminating bacon from running in background");
            handle.abort();
        }
        drop(state);
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

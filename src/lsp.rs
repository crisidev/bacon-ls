use std::{collections::HashMap, env, time::Duration};

use tokio::{fs, time::Instant};
use tower_lsp::{
    LanguageServer, jsonrpc,
    lsp_types::{
        CodeAction, CodeActionKind, CodeActionOptions, CodeActionOrCommand, CodeActionParams,
        CodeActionProviderCapability, CodeActionResponse, DeleteFilesParams, DidChangeTextDocumentParams,
        DidCloseTextDocumentParams, DidOpenTextDocumentParams, DidSaveTextDocumentParams, InitializeParams,
        InitializeResult, InitializedParams, MessageType, PositionEncodingKind, PublishDiagnosticsClientCapabilities,
        RenameFilesParams, ServerCapabilities, ServerInfo, TextDocumentClientCapabilities, TextDocumentSyncCapability,
        TextDocumentSyncKind, TextEdit, Url, WorkDoneProgressOptions, WorkspaceEdit,
    },
};

use crate::{Backend, BaconLs, Cargo, DiagnosticData, PKG_NAME, PKG_VERSION, bacon::Bacon};

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
                if let Some(value) = values.get("updateOnSaveWaitMillis") {
                    state.update_on_save_wait_millis = Duration::from_millis(
                        value
                            .as_u64()
                            .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidParams))?,
                    );
                }
                if let Some(value) = values.get("runBaconInBackgroundCommandArguments") {
                    state.run_bacon_in_background_command_args = value
                        .as_str()
                        .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidParams))?
                        .to_string();
                }
                if let Some(value) = values.get("synchronizeAllOpenFilesWaitMillis") {
                    state.syncronize_all_open_files_wait_millis = Duration::from_millis(
                        value
                            .as_u64()
                            .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidParams))?,
                    );
                }
                if let Some(value) = values.get("useCargoBackend") {
                    state.backend = if value
                        .as_bool()
                        .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidParams))?
                    {
                        Backend::Cargo
                    } else {
                        Backend::Bacon
                    };
                }
                if let Some(value) = values.get("runBaconInBackground") {
                    state.run_bacon_in_background = value
                        .as_bool()
                        .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidParams))?
                }
                if let Some(value) = values.get("validateBaconPreferences") {
                    state.validate_bacon_preferences = value
                        .as_bool()
                        .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidParams))?;
                }
                if let Some(value) = values.get("createBaconPreferencesFile") {
                    state.create_bacon_preferences_file = value
                        .as_bool()
                        .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidParams))?;
                }
                if let Some(value) = values.get("updateOnSave") {
                    state.update_on_save = value
                        .as_bool()
                        .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidParams))?;
                }
                if let Some(value) = values.get("cargoCommandArguments") {
                    state.cargo_command_args = value
                        .as_str()
                        .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidParams))?
                        .to_string();
                }
                if let Some(value) = values.get("cargoEnv") {
                    state.cargo_env = value
                        .as_str()
                        .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidParams))?
                        .split(',')
                        .map(|x| x.trim().to_owned())
                        .collect::<Vec<_>>();
                }
                if let Some(value) = values.get("updateOnChange") {
                    state.update_on_change = value
                        .as_bool()
                        .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidParams))?;
                }
                if let Some(value) = values.get("updateOnChangeCooldownMillis") {
                    state.update_on_change_cooldown_millis = Duration::from_millis(
                        value
                            .as_u64()
                            .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidParams))?,
                    );
                }
            }
        }
        if let Backend::Cargo = state.backend {
            state.run_bacon_in_background = false;
            state.validate_bacon_preferences = false;
            state.create_bacon_preferences_file = false;
            state.update_on_save = true;
            state.update_on_save_wait_millis = Duration::ZERO;
            if !state.update_on_change {
                if let Some(build_folder) = Cargo::find_git_root_directory().await {
                    state.build_folder = build_folder;
                }
            }
        }
        tracing::debug!("loaded state from lsp settings: {state:#?}");
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
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        let state = self.state.read().await;
        let run_bacon = state.run_bacon_in_background;
        let bacon_command_args = state.run_bacon_in_background_command_args.clone();
        let create_bacon_prefs = state.create_bacon_preferences_file;
        let validate_prefs = state.validate_bacon_preferences;
        let cancel_token = state.cancel_token.clone();
        let temporary_folder = state.build_folder.clone();
        let backend = state.backend;
        let update_on_change = state.update_on_change;
        let cargo_command_args = state.cargo_command_args.clone();
        let cargo_env = state.cargo_env.clone();
        drop(state);

        if let Backend::Cargo = backend {
            if update_on_change {
                if let Err(e) = Cargo::copy_source_code(&temporary_folder).await {
                    tracing::error!(
                        "error copying source code to temporary filder {}: {e}",
                        temporary_folder.display()
                    );
                }
            }
        }

        if let Some(client) = self.client.as_ref() {
            tracing::info!("{PKG_NAME} v{PKG_VERSION} lsp server initialized");
            client
                .log_message(
                    MessageType::INFO,
                    format!("{PKG_NAME} v{PKG_VERSION} lsp server initialized"),
                )
                .await;
            if let Backend::Cargo = backend {
                if update_on_change {
                    client
                        .show_message(
                            MessageType::INFO,
                            "building the first clean copy of this repo can take while",
                        )
                        .await;
                    let _ = Cargo::cargo_diagnostics(&cargo_command_args, &cargo_env, &temporary_folder).await;
                }
            }
            if validate_prefs {
                if let Err(e) = Bacon::validate_preferences(create_bacon_prefs).await {
                    tracing::error!("{e}");
                    client.show_message(MessageType::ERROR, e).await;
                }
            } else {
                tracing::warn!("skipping validation of bacon preferences, validateBaconPreferences is false");
            }

            if run_bacon {
                let mut current_dir = None;
                if let Ok(cwd) = env::current_dir() {
                    current_dir = Self::find_git_root_directory(&cwd).await;
                }
                match Bacon::run_in_background("bacon", &bacon_command_args, current_dir.as_ref(), cancel_token).await {
                    Ok(command) => {
                        tracing::info!("bacon was started successfully and is running in the background");
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
            tracing::error!("client doesn't seem to be connected, the LSP server will not function properly");
        }
        let task_state = self.state.clone();
        let task_client = self.client.clone();
        let mut guard = self.state.write().await;
        guard.sync_files_handle = Some(tokio::task::spawn(Self::syncronize_diagnostics(
            task_state,
            task_client,
        )));
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        tracing::trace!("client sent didOpen request");
        let mut state = self.state.write().await;
        let temporary_folder = state.build_folder.clone();
        let backend = state.backend;
        let update_on_change = state.update_on_change;
        state.open_files.insert(params.text_document.uri.clone());
        drop(state);
        if let Backend::Cargo = backend {
            if update_on_change {
                if let Err(e) = Cargo::copy_source_code(&temporary_folder).await {
                    tracing::error!("error copying source code to {}: {e}", temporary_folder.display());
                }
            }
        }
        self.publish_diagnostics(&params.text_document.uri).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        tracing::trace!("client sent didClose request");
        let mut state = self.state.write().await;
        let temporary_folder = state.build_folder.clone();
        let backend = state.backend;
        let update_on_change = state.update_on_change;
        state.open_files.remove(&params.text_document.uri);
        drop(state);
        if let Backend::Cargo = backend {
            if update_on_change {
                if let Err(e) = Cargo::copy_source_code(&temporary_folder).await {
                    tracing::error!("error copying source code to {}: {e}", temporary_folder.display());
                }
            }
        }
        self.publish_diagnostics(&params.text_document.uri).await;
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let state = self.state.read().await;
        let update_on_save = state.update_on_save;
        let update_on_save_wait_millis = state.update_on_save_wait_millis;
        let temporary_folder = state.build_folder.clone();
        let backend = state.backend;
        let update_on_change = state.update_on_change;
        drop(state);
        tracing::debug!(
            "client sent didSave request, updateOnSave is {update_on_save} for {} after {update_on_save_wait_millis:?}",
            params.text_document.uri
        );
        if update_on_save {
            tokio::time::sleep(update_on_save_wait_millis).await;
            if let Backend::Cargo = backend {
                if update_on_change {
                    if let Err(e) = Cargo::copy_source_code(&temporary_folder).await {
                        tracing::error!("error copying source code to {}: {e}", temporary_folder.display());
                    }
                }
            }
            self.publish_diagnostics(&params.text_document.uri).await;
        }
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let state = self.state.read().await;
        let temporary_folder = state.build_folder.clone();
        let backend = state.backend;
        let update_on_change = state.update_on_change;
        let last_change = state.last_change;
        drop(state);
        if let Backend::Cargo = backend {
            if !update_on_change {
                tracing::debug!("skipping didChange execution, update_on_change is false");
                return;
            }
            if last_change.elapsed() < Duration::from_secs(5) {
                tracing::debug!("skipping didChange execution: still in cooldown");
                return;
            }
            if let Some(source_folder) = Cargo::find_git_root_directory().await {
                let file = params.text_document.uri.path().replacen(
                    &source_folder.display().to_string(),
                    &temporary_folder.display().to_string(),
                    1,
                );
                if let Some(change) = params.content_changes.first() {
                    if change.range.is_none() && change.range_length.is_none() {
                        match fs::write(&file, &change.text).await {
                            Ok(()) => {
                                tracing::debug!(
                                    "success overriding file {file} with full changed content from LSP client"
                                );
                                self.publish_diagnostics(&params.text_document.uri).await;
                            }
                            Err(e) => tracing::debug!(
                                "error overriding file {file} with full changed content from LSP client: {e}"
                            ),
                        }
                    } else {
                        tracing::debug!("skipping overriding file {file} with changed partial content from LSP client");
                    }
                } else {
                    tracing::debug!("skipping overriding file {file}, no changed content from LSP client");
                }
                self.state.write().await.last_change = Instant::now();
            }
        } else {
            tracing::trace!("client sent didChange request, nothing to do");
        }
    }

    async fn did_delete_files(&self, params: DeleteFilesParams) {
        for file in params.files {
            tracing::debug!("client sent didDeleteFiles request for {}", file.uri);
            if let Ok(uri) = Url::parse(&file.uri) {
                let mut state = self.state.write().await;
                state.open_files.remove(&uri);
                drop(state);
            }
        }
    }

    async fn did_rename_files(&self, params: RenameFilesParams) {
        for file in params.files {
            tracing::debug!(
                "client sent didRenameFiles request {} -> {}",
                file.old_uri,
                file.new_uri
            );
            if let (Ok(old_uri), Ok(new_uri)) = (Url::parse(&file.old_uri), Url::parse(&file.new_uri)) {
                let mut state = self.state.write().await;
                let temporary_folder = state.build_folder.clone();
                let backend = state.backend;
                let update_on_change = state.update_on_change;
                state.open_files.remove(&old_uri);
                state.open_files.insert(new_uri.clone());
                drop(state);
                if let Backend::Cargo = backend {
                    if update_on_change {
                        if let Err(e) = Cargo::copy_source_code(&temporary_folder).await {
                            tracing::error!("error copying source code to {}: {e}", temporary_folder.display());
                        }
                    }
                }
                self.publish_diagnostics(&new_uri).await;
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
        state.cancel_token.cancel();
        if let Some(handle) = state.bacon_command_handle.take() {
            tracing::info!("terminating bacon from running in background");
            let _ = handle.await;
        }
        if let Some(handle) = state.sync_files_handle.take() {
            let _ = handle.await;
        }
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

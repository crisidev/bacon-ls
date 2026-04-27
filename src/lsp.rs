use std::collections::HashMap;

use ls_types::{
    CodeAction, CodeActionKind, CodeActionOptions, CodeActionOrCommand, CodeActionParams, CodeActionProviderCapability,
    CodeActionResponse, DeleteFilesParams, DidChangeConfigurationParams, DidChangeTextDocumentParams,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, DidSaveTextDocumentParams, ExecuteCommandOptions,
    ExecuteCommandParams, FileOperationFilter, FileOperationPattern, FileOperationRegistrationOptions,
    InitializeParams, InitializeResult, InitializedParams, LSPAny, MessageType, PositionEncodingKind,
    PublishDiagnosticsClientCapabilities, RenameFilesParams, ServerCapabilities, ServerInfo,
    TextDocumentClientCapabilities, TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncOptions,
    TextDocumentSyncSaveOptions, TextEdit, Uri, WorkDoneProgressOptions, WorkspaceEdit,
    WorkspaceFileOperationsServerCapabilities, WorkspaceServerCapabilities,
};
use tower_lsp_server::{LanguageServer, jsonrpc};

use crate::{
    BackendChoice, BackendRuntime, BaconLs, Cargo, CargoOptions, CorrectionEdit, DiagnosticData, PKG_NAME, PKG_VERSION,
};

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
        let mut related_information_supported = false;
        if let Some(TextDocumentClientCapabilities {
            publish_diagnostics:
                Some(PublishDiagnosticsClientCapabilities {
                    data_support,
                    related_information,
                    ..
                }),
            ..
        }) = params.capabilities.text_document
        {
            if data_support == Some(true) {
                tracing::info!("client supports diagnostics data");
                diagnostics_data_supported = true;
            } else {
                tracing::warn!("client does not support diagnostics data");
            }
            if related_information == Some(true) {
                tracing::info!("client supports related information");
                related_information_supported = true;
            } else {
                tracing::info!("client does not support related information");
            }
        } else {
            tracing::warn!("client does not support diagnostics data");
        }

        // Initialization options are the only place we can read user
        // configuration before responding to `initialize`. We need that for
        // `cargo.updateOnInsert`: the LSP capability `textDocument/didChange`
        // sync mode has to be advertised statically — clients (Neovim
        // included) don't reliably retrofit already-attached buffers when we
        // try to register it dynamically post-`initialized`.
        let init_update_on_insert = params
            .initialization_options
            .as_ref()
            .and_then(|v| v.get("cargo"))
            .and_then(|v| v.get("updateOnInsert"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if init_update_on_insert {
            tracing::info!(
                "initialization_options.cargo.updateOnInsert = true; advertising textDocument/didChange (Full sync)"
            );
        }

        let mut state = self.state.write().await;
        state.project_root = project_root;
        state.workspace_folders = params.workspace_folders;
        state.diagnostics_data_supported = diagnostics_data_supported;
        state.related_information_supported = related_information_supported;
        state.init_update_on_insert = init_update_on_insert;
        tracing::trace!("loaded state from lsp settings: {state:#?}");
        drop(state);

        // Declare didDelete/didRename so clients actually send those events
        // (handlers live in this file). The bacon backend tracks open files
        // and needs these to keep its set in sync when the user renames/deletes
        // through the file explorer. Cargo backend is unaffected but the
        // capability is cheap to advertise.
        let rust_file_filter = FileOperationFilter {
            scheme: Some("file".to_string()),
            pattern: FileOperationPattern {
                glob: "**/*.rs".to_string(),
                matches: None,
                options: None,
            },
        };
        let file_ops_registration = FileOperationRegistrationOptions {
            filters: vec![rust_file_filter],
        };
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                // Only support UTF-16 positions for now, which is the default when unspecified
                position_encoding: Some(PositionEncodingKind::UTF16),
                // Default: no change events — diagnostics come from bacon's
                // locations file or from cargo's JSON output. The cargo
                // backend's `updateOnInsert` mode flips this to Full when the
                // user opts in via `initialization_options.cargo.updateOnInsert`,
                // so non-users never pay for buffer-shipping.
                text_document_sync: Some(TextDocumentSyncCapability::Options(TextDocumentSyncOptions {
                    open_close: Some(true),
                    change: Some(if init_update_on_insert {
                        TextDocumentSyncKind::FULL
                    } else {
                        TextDocumentSyncKind::NONE
                    }),
                    save: Some(TextDocumentSyncSaveOptions::Supported(true)),
                    ..Default::default()
                })),
                code_action_provider: Some(CodeActionProviderCapability::Options(CodeActionOptions {
                    code_action_kinds: Some(vec![CodeActionKind::QUICKFIX]),
                    work_done_progress_options: WorkDoneProgressOptions {
                        work_done_progress: Some(false),
                    },
                    resolve_provider: None,
                })),
                execute_command_provider: Some(ExecuteCommandOptions {
                    commands: vec!["bacon_ls.run".to_string()],
                    ..Default::default()
                }),
                workspace: Some(WorkspaceServerCapabilities {
                    workspace_folders: None,
                    file_operations: Some(WorkspaceFileOperationsServerCapabilities {
                        did_rename: Some(file_ops_registration.clone()),
                        did_delete: Some(file_ops_registration),
                        ..Default::default()
                    }),
                }),
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

        let mut state = self.state.write().await;
        if state.backend.is_none() {
            // No workspace/configuration response (or empty). Still honor
            // the init-options seed so live mode works for clients that only
            // provide settings via `initialization_options`.
            let mut config = CargoOptions::default();
            if state.init_update_on_insert {
                config.update_on_insert = true;
            }
            if let Err(e) = Self::init_cargo_backend(&mut state, config) {
                tracing::error!("{e}");
                drop(state);
                self.client.show_message(MessageType::ERROR, e).await;
                return;
            }
        }
        let backend_chosen = state
            .backend
            .as_ref()
            .expect("backend initialized above")
            .backend_choice();
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
            tracing::info!("triggering initial cargo diagnostics");
            self.publish_cargo_diagnostics().await
        }
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        tracing::info!("client sent didChangeConfiguration");
        if let Some(settings) = params.settings.as_object()
            && !settings.is_empty()
        {
            if let Some(settings) = settings.get("bacon_ls") {
                tracing::debug!("using client provided settings");
                self.adapt_to_settings(settings).await;
            }
        } else {
            tracing::debug!("settings is either not an object or is empty");
            self.pull_configuration().await;
        }
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        tracing::trace!("client sent didOpen request");
        let mut state = self.state.write().await;
        match &mut state.backend {
            Some(BackendRuntime::Bacon { runtime, .. }) => {
                runtime.open_files.insert(params.text_document.uri.clone());
                drop(state);
                self.publish_bacon_diagnostics(&params.text_document.uri).await;
            }
            Some(BackendRuntime::Cargo { runtime, .. }) => {
                // Debounce against the initial cargo run: on client startup,
                // `initialized` kicks off a run and the first `didOpen`
                // arrives in the same flurry. Skipping here lets the in-flight
                // run complete instead of being cancelled and restarted.
                if let Some(ts) = runtime.last_run_started
                    && ts.elapsed() < std::time::Duration::from_secs(1)
                {
                    tracing::trace!("did_open within debounce window of last cargo trigger; skipping");
                    return;
                }
                drop(state);
                self.publish_cargo_diagnostics().await;
            }
            None => {}
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        tracing::trace!("client sent didClose request");
        let mut state = self.state.write().await;
        if let Some(BackendRuntime::Bacon { runtime, .. }) = &mut state.backend {
            runtime.open_files.remove(&params.text_document.uri);
            drop(state);
            self.publish_bacon_diagnostics(&params.text_document.uri).await;
            return;
        }
        drop(state);
        // Cargo backend with live shadow: revert any dirty buffer for the
        // closed file back to a hardlink so subsequent live runs read the
        // on-disk version.
        self.restore_shadow_link_if_dirty(&params.text_document.uri).await;
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
            BackendRuntime::Cargo { config, .. } => {
                let check_on_save = config.check_on_save;
                drop(state);
                // A pending live run would race with the canonical save run
                // and publish stale (pre-save) shadow diagnostics on top.
                // Cancel it before doing anything else.
                self.cancel_live_debounce().await;
                // Save makes the shadow's dirty override stale: the on-disk
                // file now matches what the user wants checked. Restore the
                // hardlink before the cargo run so the live target dir picks
                // up the saved content next time it's used.
                self.restore_shadow_link_if_dirty(&params.text_document.uri).await;
                if check_on_save {
                    self.publish_cargo_diagnostics().await;
                }
            }
        }
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        // Live mode is only meaningful for the cargo backend; the bacon
        // backend reads diagnostics from a file written by an external bacon
        // process. Bail early to keep this hot path cheap when disabled.
        let live_on = {
            let state = self.state.read().await;
            matches!(
                &state.backend,
                Some(BackendRuntime::Cargo { config, .. }) if config.update_on_insert
            )
        };
        if !live_on {
            tracing::debug!("did_change ignored: updateOnInsert is off");
            return;
        }
        tracing::info!(
            uri = params.text_document.uri.as_str(),
            changes = params.content_changes.len(),
            "did_change received (live mode)"
        );

        // We register the change capability dynamically with `Full` sync
        // (one entry, range = None, full text). Anything else is a client
        // mismatch — log and skip rather than guess.
        let Some(content) = params.content_changes.into_iter().find(|c| c.range.is_none()) else {
            tracing::warn!("did_change without full-sync content; client may not honor dynamic registration");
            return;
        };

        self.live_update_dirty(params.text_document.uri, content.text).await;
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

        if !diagnostics_data_supported {
            return Ok(None);
        }

        let bacon_ls = "bacon-ls".to_string();
        let actions = params
            .context
            .diagnostics
            .iter()
            .filter(|diag| diag.source.as_ref() == Some(&bacon_ls))
            .flat_map(|diag| match &diag.data {
                Some(data) => {
                    if let Ok(DiagnosticData { corrections }) = serde_json::from_value::<DiagnosticData>(data.clone()) {
                        corrections
                            .iter()
                            .map(|c| {
                                CodeActionOrCommand::CodeAction(CodeAction {
                                    title: c.label.clone(),
                                    kind: Some(CodeActionKind::QUICKFIX),
                                    diagnostics: Some(vec![diag.clone()]),
                                    edit: Some(WorkspaceEdit {
                                        changes: Some(HashMap::from([(
                                            params.text_document.uri.clone(),
                                            c.edits
                                                .iter()
                                                .map(|e: &CorrectionEdit| TextEdit {
                                                    range: e.range,
                                                    new_text: e.new_text.clone(),
                                                })
                                                .collect(),
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
    }

    async fn execute_command(&self, params: ExecuteCommandParams) -> jsonrpc::Result<Option<LSPAny>> {
        if params.command == "bacon_ls.run" {
            let state = self.state.read().await;
            if let Some(BackendRuntime::Cargo { .. }) = state.backend.as_ref() {
                drop(state);
                self.publish_cargo_diagnostics().await;
            }
            return Ok(None);
        }

        Err(jsonrpc::Error::method_not_found())
    }

    async fn shutdown(&self) -> jsonrpc::Result<()> {
        tracing::info!("shutdown requested");
        let mut state = self.state.write().await;
        let backend = state.backend.take();
        drop(state);

        if let Some(backend) = backend {
            match backend {
                BackendRuntime::Bacon { mut runtime, .. } => {
                    runtime.shutdown_token.cancel();
                    // Cap each await so a stuck bacon subprocess or watcher can't
                    // keep the LSP alive past the client's restart deadline.
                    let deadline = std::time::Duration::from_secs(2);
                    if let Some(handle) = runtime.command_handle.take() {
                        tracing::info!("terminating bacon from running in background");
                        match tokio::time::timeout(deadline, handle).await {
                            Ok(Ok(())) => {}
                            Ok(Err(e)) => {
                                tracing::warn!("bacon command task failed during shutdown: {e}")
                            }
                            Err(_) => tracing::warn!("bacon command task timed out during shutdown"),
                        }
                    }
                    let sync_handle = runtime.sync_files_handle;
                    match tokio::time::timeout(deadline, sync_handle).await {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => tracing::warn!("sync files task failed during shutdown: {e}"),
                        Err(_) => tracing::warn!("sync files task timed out during shutdown"),
                    }
                }
                BackendRuntime::Cargo { mut runtime, .. } => {
                    runtime.cancel_token.cancel();
                    // Abort any pending live debounced trigger so the spawned
                    // sleep doesn't outlive the server and try to invoke
                    // cargo against a torn-down backend.
                    if let Some(handle) = runtime.live_debounce.take() {
                        handle.abort();
                    }
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

        // Force-exit watchdog. An in-flight server-to-client request (e.g. the
        // `workspace/configuration` we issue from `initialized`) can keep
        // `Server::serve()` alive past the `exit` notification, because there's
        // no way to cancel a waiter on a client response that will never
        // arrive. Without this, `:LspRestart` in Neovim sees the server never
        // die and gives up on spawning a fresh instance.
        tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            std::process::exit(0);
        });

        Ok(())
    }
}

use std::time::Duration;

use tower_lsp::{
    jsonrpc,
    lsp_types::{
        DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
        DidSaveTextDocumentParams, InitializeParams, InitializeResult, InitializedParams,
        MessageType, PositionEncodingKind, PublishDiagnosticsClientCapabilities,
        ServerCapabilities, ServerInfo, TextDocumentClientCapabilities, TextDocumentSyncCapability,
        TextDocumentSyncKind,
    },
    LanguageServer,
};

use crate::{BaconLs, PKG_NAME, PKG_VERSION};

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
                        .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InternalError))?
                        .to_string();
                }
                if let Some(value) = values.get("updateOnSave") {
                    state.update_on_save = value
                        .as_bool()
                        .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InternalError))?;
                }
                if let Some(value) = values.get("updateOnSaveWaitMillis") {
                    state.update_on_save_wait_millis = Duration::from_millis(
                        value
                            .as_u64()
                            .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InternalError))?,
                    );
                }
                if let Some(value) = values.get("updateOnChange") {
                    state.update_on_change = value
                        .as_bool()
                        .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InternalError))?;
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
        tracing::debug!("client sent didSave request, update_on_save is {update_on_save} after waiting bacon for {update_on_save_wait_millis:?}");
        if update_on_save {
            tokio::time::sleep(update_on_save_wait_millis).await;
            self.publish_diagnostics(&params.text_document.uri).await;
        }
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let update_on_change = self.state.read().await.update_on_change;
        tracing::debug!("client sent didChange request, update_on_change is {update_on_change}");
        if update_on_change {
            self.publish_diagnostics(&params.text_document.uri).await;
        }
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

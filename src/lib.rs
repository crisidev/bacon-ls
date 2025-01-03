//! Bacon Language Server
use std::collections::HashMap;
use std::env;
use std::path::Path;
use std::time::Duration;

use argh::FromArgs;
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::RwLock;
use tower_lsp::{jsonrpc, Client, LanguageServer};
use tower_lsp::{lsp_types::*, LspService, Server};
use tracing_subscriber::fmt::format::FmtSpan;

const PKG_NAME: &str = env!("CARGO_PKG_NAME");
pub const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");
const LOCATIONS_FILE: &str = ".bacon-locations";

/// bacon-ls - https://github.com/crisidev/bacon-ls
#[derive(Debug, FromArgs)]
pub struct Args {
    /// display version information
    #[argh(switch, short = 'v')]
    pub version: bool,
}

#[derive(Debug)]
struct State {
    workspace_folders: Option<Vec<WorkspaceFolder>>,
    locations_file: String,
    update_on_save: bool,
    update_on_save_wait_millis: Duration,
    update_on_change: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            workspace_folders: None,
            locations_file: LOCATIONS_FILE.to_string(),
            update_on_save: true,
            update_on_save_wait_millis: Duration::from_millis(1000),
            update_on_change: true,
        }
    }
}

#[derive(Debug)]
pub struct BaconLs {
    client: Option<Client>,
    state: RwLock<State>,
}

impl Default for BaconLs {
    fn default() -> Self {
        Self {
            client: None,
            state: RwLock::new(State::default()),
        }
    }
}

impl BaconLs {
    fn new(client: Client) -> Self {
        Self {
            client: Some(client),
            state: RwLock::new(State::default()),
        }
    }

    fn configure_tracing(log_level: Option<String>) {
        // Configure logging to file.
        let level = log_level.unwrap_or_else(|| env::var("RUST_LOG").unwrap_or("off".to_string()));
        if level != "off" {
            tracing_subscriber::fmt()
                .with_env_filter(level)
                .with_writer(
                    std::fs::OpenOptions::new()
                        .create(true)
                        .write(true)
                        .truncate(true)
                        .open(format!("{PKG_NAME}.log"))
                        .unwrap(),
                )
                .with_thread_names(true)
                .with_span_events(FmtSpan::CLOSE)
                .with_line_number(true)
                .with_target(false)
                .compact()
                .init();
        }
    }

    /// Run the LSP server.
    pub async fn serve() {
        Self::configure_tracing(None);
        // Lock stdin / stdout.
        let stdin = tokio::io::stdin();
        let stdout = tokio::io::stdout();
        // Start the service.
        let (service, socket) = LspService::new(Self::new);
        Server::new(stdin, stdout, socket).serve(service).await;
    }

    async fn diagnostics(&self, uri: Option<&Url>) -> Vec<(Url, Diagnostic)> {
        let state = self.state.read().await;
        let locations_file = state.locations_file.clone();
        let workspace_folders = state.workspace_folders.clone();
        drop(state);
        let mut diagnostics: Vec<(Url, Diagnostic)> = vec![];
        if let Some(workspace_folders) = workspace_folders.as_ref() {
            for folder in workspace_folders.iter() {
                let folder_path = Path::new(folder.uri.path());
                let bacon_locations = folder_path.join(&locations_file);

                match File::open(&bacon_locations).await {
                    Ok(fd) => {
                        let reader = BufReader::new(fd);
                        let mut lines = reader.lines();
                        while let Some(line) = lines.next_line().await.unwrap_or_else(|e| {
                            tracing::error!(
                                "error reading line from file {}: {e}",
                                bacon_locations.display()
                            );
                            None
                        }) {
                            if let Some((path, diagnostic)) =
                                Self::parse_bacon_diagnostic_line_with_spans(
                                    &line,
                                    folder_path,
                                    uri,
                                )
                            {
                                let mut exists = false;
                                for (existing_path, existing_diagnostic) in diagnostics.iter() {
                                    if existing_path.path() == path.path()
                                        && diagnostic.range == existing_diagnostic.range
                                        && diagnostic.severity == existing_diagnostic.severity
                                        && diagnostic.message == existing_diagnostic.message
                                    {
                                        tracing::debug!(
                                            "deduplicating existing diagnostic {diagnostic:?}"
                                        );
                                        exists = true;
                                        break;
                                    }
                                }
                                if !exists {
                                    diagnostics.push((path, diagnostic));
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("unable to read file {}: {e}", bacon_locations.display())
                    }
                }
            }
        }
        diagnostics
    }

    async fn diagnostics_vec(&self, uri: Option<&Url>) -> Vec<Diagnostic> {
        self.diagnostics(uri)
            .await
            .into_iter()
            .map(|(_, y)| y)
            .collect::<Vec<Diagnostic>>()
    }

    async fn diagnostics_map(&self, uri: Option<&Url>) -> HashMap<Url, Vec<Diagnostic>> {
        let mut map: HashMap<Url, Vec<Diagnostic>> = HashMap::new();
        for (path, diagnostic) in self.diagnostics(uri).await {
            if let Some(el) = map.get_mut(&path) {
                el.push(diagnostic);
            } else {
                map.insert(path, vec![diagnostic]);
            }
        }
        map
    }

    async fn publish_diagnostics(&self, uri: &Url) {
        if let Some(client) = self.client.as_ref() {
            client
                .publish_diagnostics(uri.clone(), self.diagnostics_vec(Some(uri)).await, None)
                .await;
        }
    }

    fn parse_bacon_diagnostic_line_with_spans(
        line: &str,
        folder_path: &Path,
        uri: Option<&Url>,
    ) -> Option<(Url, Diagnostic)> {
        let line_split: Vec<&str> = line.splitn(7, ':').collect();
        if line_split.len() == 7 {
            let severity = match line_split[0] {
                "warning" => DiagnosticSeverity::WARNING,
                "info" | "information" | "note" => DiagnosticSeverity::INFORMATION,
                "hint" | "help" => DiagnosticSeverity::HINT,
                _ => DiagnosticSeverity::ERROR,
            };
            let file_path = folder_path.join(line_split[1]);
            let line_start = line_split[2].parse().unwrap_or(1);
            let line_end = line_split[3].parse().unwrap_or(1);
            let column_start = line_split[4].parse().unwrap_or(1);
            let column_end = line_split[5].parse().unwrap_or(1);
            match format!("file://{}", file_path.display()).parse::<Url>() {
                Ok(path) => {
                    if uri.is_none() || Some(&path) == uri {
                        let message = line_split[6].replace("\\n", "\n");
                        tracing::debug!("new diagnostic: severity: {severity:?}, path: {path}, line_start: {line_start}, line_end: {line_end}, column_start: {column_start}, column_end: {column_end}, message: {message}");
                        return Some((
                            path,
                            Diagnostic {
                                range: Range::new(
                                    Position::new((line_start - 1) as u32, column_start - 1),
                                    Position::new((line_end - 1) as u32, column_end - 1),
                                ),
                                severity: Some(severity),
                                source: Some(PKG_NAME.to_string()),
                                message,
                                ..Diagnostic::default()
                            },
                        ));
                    } else {
                        tracing::debug!(
                            "request diagnostic file path {uri:?} doesn't match bacon file path {path}"
                        );
                    }
                }
                Err(e) => {
                    tracing::error!("error parsing file {} path: {e}", file_path.display())
                }
            }
        } else {
            tracing::error!(
                "error extracting bacon diagnostic, malformed severity:path:line_start:line_end:column_start:column_end:message"
            );
        }
        None
    }
}

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
                diagnostic_provider: Some(DiagnosticServerCapabilities::Options(
                    DiagnosticOptions {
                        workspace_diagnostics: true,
                        ..Default::default()
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
        }
    }

    async fn workspace_diagnostic(
        &self,
        _params: WorkspaceDiagnosticParams,
    ) -> jsonrpc::Result<WorkspaceDiagnosticReportResult> {
        tracing::debug!("client sent workspaceDiagnostics request");
        let mut diagnostics = Vec::new();
        for (path, diagnostic) in self.diagnostics_map(None).await {
            tracing::debug!(
                "updating {} workspace diagnostics for {path}",
                diagnostic.len(),
            );
            diagnostics.push(WorkspaceDocumentDiagnosticReport::Full(
                WorkspaceFullDocumentDiagnosticReport {
                    uri: path,
                    version: None,
                    full_document_diagnostic_report: FullDocumentDiagnosticReport {
                        result_id: None,
                        items: diagnostic,
                    },
                },
            ));
        }
        Ok(WorkspaceDiagnosticReportResult::Report(
            WorkspaceDiagnosticReport { items: diagnostics },
        ))
    }

    async fn diagnostic(
        &self,
        params: DocumentDiagnosticParams,
    ) -> jsonrpc::Result<DocumentDiagnosticReportResult> {
        tracing::debug!("client sent diagnostics request");
        let diagnostics = self.diagnostics_vec(Some(&params.text_document.uri)).await;
        let client = self
            .client
            .as_ref()
            .ok_or_else(|| jsonrpc::Error::new(jsonrpc::ErrorCode::InternalError))?;

        tracing::debug!(
            "updating {} document diagnostic for {}",
            diagnostics.len(),
            params.text_document.uri
        );
        client
            .publish_diagnostics(params.text_document.uri.clone(), Vec::new(), None)
            .await;
        Ok(DocumentDiagnosticReportResult::Report(
            DocumentDiagnosticReport::Full(RelatedFullDocumentDiagnosticReport {
                related_documents: None,
                full_document_diagnostic_report: FullDocumentDiagnosticReport {
                    result_id: None,
                    items: diagnostics,
                },
            }),
        ))
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

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;
    use pretty_assertions::assert_eq;

    const ERROR_LINE_WITH_SPANS: &str = "error:/app/github/bacon-ls/src/lib.rs:352:352:9:20:cannot find value `one` in this scope\n    |\n352 |         one\n    |         ^^^ help: a unit variant with a similar name exists: `None`\n    |\n   ::: /Users/matteobigoi/.rustup/toolchains/stable-aarch64-apple-darwin/lib/rustlib/src/rust/library/core/src/option.rs:576:5\n    |\n576 |     None,\n    |     ---- similarly named unit variant `None` defined here\n\nFor more information about this error, try `rustc --explain E0425`.\nerror: could not compile `bacon-ls` (lib) due to 1 previous error";

    #[test]
    fn test_parse_bacon_diagnostic_line_with_spans_ok() {
        let result = BaconLs::parse_bacon_diagnostic_line_with_spans(
            ERROR_LINE_WITH_SPANS,
            Path::new("/app/github/bacon-ls"),
            None,
        );
        let (url, diagnostic) = result.unwrap();
        assert_eq!(url.to_string(), "file:///app/github/bacon-ls/src/lib.rs");
        assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(diagnostic.source, Some(PKG_NAME.to_string()));
        assert_eq!(
            diagnostic.message,
            r#"cannot find value `one` in this scope
    |
352 |         one
    |         ^^^ help: a unit variant with a similar name exists: `None`
    |
   ::: /Users/matteobigoi/.rustup/toolchains/stable-aarch64-apple-darwin/lib/rustlib/src/rust/library/core/src/option.rs:576:5
    |
576 |     None,
    |     ---- similarly named unit variant `None` defined here

For more information about this error, try `rustc --explain E0425`.
error: could not compile `bacon-ls` (lib) due to 1 previous error"#
        );
        let result = BaconLs::parse_bacon_diagnostic_line_with_spans(
            ERROR_LINE_WITH_SPANS,
            Path::new("/app/github/bacon-ls"),
            Some(&Url::from_str("file:///app/github/bacon-ls/src/lib.rs").unwrap()),
        );
        let (url, diagnostic) = result.unwrap();
        assert_eq!(url.to_string(), "file:///app/github/bacon-ls/src/lib.rs");
        assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(diagnostic.source, Some(PKG_NAME.to_string()));
    }

    #[test]
    fn test_parse_bacon_diagnostic_line_with_spans_ko() {
        // Different path
        let result = BaconLs::parse_bacon_diagnostic_line_with_spans(
            ERROR_LINE_WITH_SPANS,
            Path::new("/app/github/bacon-ls"),
            Some(&Url::from_str("file:///app/github/bacon-ls/src/main.rs").unwrap()),
        );
        assert_eq!(result, None);

        // Non parsable path
        let result = BaconLs::parse_bacon_diagnostic_line_with_spans(
            ERROR_LINE_WITH_SPANS,
            Path::new("/app/github/bacon-ls"),
            Some(&Url::from_str("fle://app/github/bacon-ls/src/main.rs").unwrap()),
        );
        assert_eq!(result, None);

        // Unparsable line
        let result = BaconLs::parse_bacon_diagnostic_line_with_spans(
            "warning:/file:1:1",
            Path::new("/app/github/bacon-ls"),
            Some(&Url::from_str("fle://app/github/bacon-ls/src/main.rs").unwrap()),
        );
        assert_eq!(result, None);

        // Empty line
        let result = BaconLs::parse_bacon_diagnostic_line_with_spans(
            "",
            Path::new("/app/github/bacon-ls"),
            Some(&Url::from_str("fle://app/github/bacon-ls/src/main.rs").unwrap()),
        );
        assert_eq!(result, None);
    }
}

//! Bacon Language Server
use std::collections::HashMap;
use std::env;
use std::path::Path;

use argh::FromArgs;
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::RwLock;
use tower_lsp::{jsonrpc, Client, LanguageServer};
use tower_lsp::{lsp_types::*, LspService, Server};
use tracing_subscriber::fmt::format::FmtSpan;

const PKG_NAME: &str = env!("CARGO_PKG_NAME");
pub const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");
const PKG_AUTHORS: &str = env!("CARGO_PKG_AUTHORS");
const LOCATIONS_FILE: &str = ".bacon-locations";
const BACON_COMMAND: &str = "bacon clippy -- --all-features";

/// {PKG_NAME} v{PKG_VERSION} - {PKG_AUTHORS}
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
    spawn_bacon: bool,
    spawn_bacon_command: String,
}

impl Default for State {
    fn default() -> Self {
        Self {
            workspace_folders: None,
            locations_file: LOCATIONS_FILE.to_string(),
            spawn_bacon: false,
            spawn_bacon_command: BACON_COMMAND.to_string(),
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
        let mut diagnostics = vec![];
        if let Some(workspace_folders) = workspace_folders.as_ref() {
            for folder in workspace_folders.iter() {
                let bacon_locations = Path::new(&folder.name).join(&locations_file);
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
                                Self::parse_bacon_diagnostic_line(&line, uri)
                            {
                                diagnostics.push((path, diagnostic));
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

    fn parse_bacon_diagnostic_line(line: &str, uri: Option<&Url>) -> Option<(Url, Diagnostic)> {
        let line_split: Vec<&str> = line.splitn(5, ':').collect();
        if line_split.len() == 5 {
            let severity = match line_split[0] {
                "warning" => DiagnosticSeverity::WARNING,
                "info" | "information" => DiagnosticSeverity::INFORMATION,
                "hint" => DiagnosticSeverity::HINT,
                _ => DiagnosticSeverity::ERROR,
            };
            let path = line_split[1];
            let line = line_split[2].parse().unwrap_or(1);
            let col = line_split[3].parse().unwrap_or(1);
            match format!("file://{}", path).parse::<Url>() {
                Ok(path) => {
                    if uri.is_none() || Some(&path) == uri {
                        let message = line_split[4].replace("\\n", "\n");
                        tracing::debug!("new diagnostic: severity: {severity:?}, path: {path}, line: {line}, col: {col}, message: {message}");
                        return Some((
                            path,
                            Diagnostic {
                                range: Range::new(
                                    Position::new((line - 1) as u32, col - 1),
                                    Position::new((line - 1) as u32, col + 4),
                                ),
                                severity: Some(severity),
                                source: Some(PKG_NAME.to_string()),
                                message,
                                ..Diagnostic::default()
                            },
                        ));
                    } else {
                        tracing::debug!(
                            "request diagnostic file path doesn't match bacon file path"
                        );
                    }
                }
                Err(e) => {
                    tracing::error!("error parsing file {path} path: {e}")
                }
            }
        } else {
            tracing::error!(
                "error extracting bacon diagnostic, malformed level:path:line:col:message"
            );
        }
        None
    }

    async fn spawn_bacon(&self) {
        // Create an mpsc channel to send output
        let guard = self.state.read().await;
        let bacon_command = guard.spawn_bacon_command.clone();
        drop(guard);
        let mut args: Vec<String> = bacon_command
            .split_whitespace()
            .map(|c| c.to_string())
            .collect();
        let command = args.remove(0);
        // Spawn a task to run the command
        tracing::info!("spawing bacon in background with command {bacon_command}");
        tokio::spawn(async move {
            let mut child = Command::new(command)
                .args(args)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("Failed to spawn child process");

            let stdout = child.stdout.take().expect("Failed to get stdout");
            let stderr = child.stderr.take().expect("Failed to get stderr");

            let mut stdout_reader = BufReader::new(stdout).lines();
            let mut stderr_reader = BufReader::new(stderr).lines();

            // Stream stdout and stderr
            loop {
                tokio::select! {
                    Ok(Some(line)) = stdout_reader.next_line() => {
                        tracing::info!("[bacon stdout] {line}");
                    }
                    Ok(Some(line)) = stderr_reader.next_line() => {
                        tracing::info!("[bacon stderr] {line}");
                    }
                    else => break, // EOF
                }
            }

            // Wait for the child process to exit
            let _ = child.wait().await;
        });
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for BaconLs {
    #[tracing::instrument(skip_all)]
    async fn initialize(&self, params: InitializeParams) -> jsonrpc::Result<InitializeResult> {
        tracing::info!("initializing {PKG_NAME} v{PKG_VERSION}",);

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
        let spawn_bacon = state.spawn_bacon;
        state.workspace_folders = params.workspace_folders;

        if let Some(ops) = params.initialization_options {
            if let Some(values) = ops.as_object() {
                if let Some(value) = values.get("locationsFile") {
                    state.locations_file = value
                        .as_str()
                        .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InternalError))?
                        .to_string();
                }
                if let Some(value) = values.get("spawnBacon") {
                    state.spawn_bacon = value
                        .as_bool()
                        .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InternalError))?;
                }
                if let Some(value) = values.get("spawnBaconCommand") {
                    state.spawn_bacon_command = value
                        .as_str()
                        .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InternalError))?
                        .to_string();
                }
            }
        }
        tracing::debug!("loaded state from lsp settings: {state:#?}");
        drop(state);

        if spawn_bacon {
            self.spawn_bacon().await;
        }

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

    #[tracing::instrument(skip_all)]
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

    #[tracing::instrument(skip_all)]
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

    #[tracing::instrument(skip_all)]
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

    #[tracing::instrument(skip_all)]
    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        tracing::debug!("client sent didOpen request");
        if let Some(client) = self.client.as_ref() {
            client
                .publish_diagnostics(
                    params.text_document.uri.clone(),
                    self.diagnostics_vec(Some(&params.text_document.uri)).await,
                    None,
                )
                .await;
        }
    }

    #[tracing::instrument(skip_all)]
    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        tracing::debug!("client sent didClose request");
        if let Some(client) = self.client.as_ref() {
            client
                .publish_diagnostics(
                    params.text_document.uri.clone(),
                    self.diagnostics_vec(Some(&params.text_document.uri)).await,
                    None,
                )
                .await;
        }
    }

    #[tracing::instrument(skip_all)]
    async fn did_change(&self, _: DidChangeTextDocumentParams) {
        tracing::debug!("client sent didChange request");
    }

    #[tracing::instrument(skip_all)]
    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        tracing::debug!("client sent didSave request");
        if let Some(client) = self.client.as_ref() {
            client
                .publish_diagnostics(
                    params.text_document.uri.clone(),
                    self.diagnostics_vec(Some(&params.text_document.uri)).await,
                    None,
                )
                .await;
        }
    }

    #[tracing::instrument(skip_all)]
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

    const ERROR_LINE: &str = "error:/app/github/bacon-ls/src/lib.rs:352:9:cannot find value `one` in this scope\n    |\n352 |         one\n    |         ^^^ help: a unit variant with a similar name exists: `None`\n    |\n   ::: /Users/matteobigoi/.rustup/toolchains/stable-aarch64-apple-darwin/lib/rustlib/src/rust/library/core/src/option.rs:576:5\n    |\n576 |     None,\n    |     ---- similarly named unit variant `None` defined here\n\nFor more information about this error, try `rustc --explain E0425`.\nerror: could not compile `bacon-ls` (lib) due to 1 previous error";

    #[test]
    fn test_parse_bacon_diagnostic_line_ok() {
        let result = BaconLs::parse_bacon_diagnostic_line(ERROR_LINE, None);
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
        let result = BaconLs::parse_bacon_diagnostic_line(
            ERROR_LINE,
            Some(&Url::from_str("file:///app/github/bacon-ls/src/lib.rs").unwrap()),
        );
        let (url, diagnostic) = result.unwrap();
        assert_eq!(url.to_string(), "file:///app/github/bacon-ls/src/lib.rs");
        assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(diagnostic.source, Some(PKG_NAME.to_string()));
    }

    #[test]
    fn test_parse_bacon_diagnostic_line_ko() {
        // Different path
        let result = BaconLs::parse_bacon_diagnostic_line(
            ERROR_LINE,
            Some(&Url::from_str("file:///app/github/bacon-ls/src/main.rs").unwrap()),
        );
        assert_eq!(result, None);

        // Non parsable path
        let result = BaconLs::parse_bacon_diagnostic_line(
            ERROR_LINE,
            Some(&Url::from_str("fle://app/github/bacon-ls/src/main.rs").unwrap()),
        );
        assert_eq!(result, None);

        // Unparsable line
        let result = BaconLs::parse_bacon_diagnostic_line(
            "warning:/file:1:1",
            Some(&Url::from_str("fle://app/github/bacon-ls/src/main.rs").unwrap()),
        );
        assert_eq!(result, None);

        // Empty line
        let result = BaconLs::parse_bacon_diagnostic_line(
            "",
            Some(&Url::from_str("fle://app/github/bacon-ls/src/main.rs").unwrap()),
        );
        assert_eq!(result, None);
    }
}

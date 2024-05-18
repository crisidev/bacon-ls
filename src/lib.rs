//! Bacon Language Server
use std::collections::HashMap;
use std::env;
use std::path::Path;
use std::time::{Duration, SystemTime};
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::Mutex;

use serde_json::to_string;
use tokio::time::Instant;
use tower_lsp::{jsonrpc, Client, LanguageServer};
use tower_lsp::{lsp_types::*, LspService, Server};
use tracing_subscriber::fmt::format::FmtSpan;

pub const PKG_NAME: &str = env!("CARGO_PKG_NAME");
pub(crate) const LOCATIONS_FILE: &str = ".bacon-locations";
pub(crate) const WAIT_TIME_SECONDS: u64 = 10;

#[derive(Default)]
pub(crate) struct State {
    workspace_folders: Option<Vec<WorkspaceFolder>>,
    locations_file: String,
    wait_time: Duration,
}

pub struct BaconLs {
    client: Client,
    state: Mutex<State>,
}

impl BaconLs {
    fn new(client: Client) -> Self {
        Self {
            client,
            state: Mutex::new(State::default()),
        }
    }

    pub async fn serve() {
        let level = env::var("RUST_LOG").unwrap_or_else(|_| "off".to_string());
        if level != "off" {
            tracing_subscriber::fmt()
                .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
                .with_writer(
                    std::fs::OpenOptions::new()
                        .create(true)
                        .write(true)
                        .truncate(true)
                        .open(format!("{PKG_NAME}.log"))
                        .unwrap(),
                )
                .with_span_events(FmtSpan::CLOSE)
                .init();
        }
        let stdin = tokio::io::stdin();
        let stdout = tokio::io::stdout();
        let (service, socket) = LspService::new(Self::new);
        Server::new(stdin, stdout, socket).serve(service).await;
    }

    async fn diagnostics(&self, uri: Option<&Url>) -> Vec<(Url, Diagnostic)> {
        let state = self.state.lock().await;
        let locations_file = state.locations_file.clone();
        let workspace_folders = state.workspace_folders.clone();
        let wait_time = state.wait_time;
        drop(state);
        Self::wait_for_bacon(&locations_file, wait_time).await;
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

    async fn bacon_locations_file_modified(locations_file: &str) -> SystemTime {
        match File::open(locations_file).await {
            Ok(fd) => match fd.metadata().await {
                Ok(meta) => meta.modified().unwrap_or_else(|_| SystemTime::now()),
                Err(e) => {
                    tracing::error!("error reading file metadata: {e}");
                    SystemTime::now()
                }
            },
            Err(e) => {
                tracing::error!("error reading file metadata: {e}");
                SystemTime::now()
            }
        }
    }

    async fn wait_for_bacon(locations_file: &str, wait_time: Duration) {
        let last_modified = Self::bacon_locations_file_modified(locations_file).await;
        let start = Instant::now();
        while last_modified == Self::bacon_locations_file_modified(&locations_file).await {
            if start.elapsed() > wait_time {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
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
}

#[tower_lsp::async_trait]
impl LanguageServer for BaconLs {
    async fn initialize(&self, params: InitializeParams) -> jsonrpc::Result<InitializeResult> {
        tracing::info!(
            "initializing {PKG_NAME}, params: {}",
            to_string(&params).unwrap_or_default()
        );

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

        let mut state = self.state.lock().await;
        state.workspace_folders = params.workspace_folders;

        if let Some(ops) = params.initialization_options {
            if let Some(values) = ops.as_object() {
                if let Some(value) = values.get("locationsFile").cloned() {
                    state.locations_file = value.as_str().unwrap_or("").to_lowercase();
                } else {
                    state.locations_file = LOCATIONS_FILE.to_string();
                }
                if let Some(value) = values.get("waitTimeSeconds").cloned() {
                    state.wait_time =
                        Duration::from_secs(value.as_u64().unwrap_or(WAIT_TIME_SECONDS));
                } else {
                    state.wait_time = Duration::from_secs(WAIT_TIME_SECONDS);
                }
            }
        }

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                // only support UTF-16 positions for now, which is the default when unspecified
                position_encoding: Some(PositionEncodingKind::UTF16),
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                diagnostic_provider: Some(DiagnosticServerCapabilities::Options(
                    DiagnosticOptions {
                        // workspace_diagnostics: true,
                        ..Default::default()
                    },
                )),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: PKG_NAME.to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
        })
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        self.client
            .publish_diagnostics(
                params.text_document.uri.clone(),
                self.diagnostics_vec(Some(&params.text_document.uri)).await,
                None,
            )
            .await;
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, format!("{PKG_NAME} server initialized!"))
            .await;
    }

    async fn workspace_diagnostic(
        &self,
        _params: WorkspaceDiagnosticParams,
    ) -> jsonrpc::Result<WorkspaceDiagnosticReportResult> {
        let mut diagnostics = Vec::new();
        for (path, diagnostic) in self.diagnostics_map(None).await {
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
        self.client
            .log_message(
                MessageType::INFO,
                format!(
                    "updating {} workspace diagnostic documents",
                    diagnostics.len(),
                ),
            )
            .await;

        Ok(WorkspaceDiagnosticReportResult::Report(
            WorkspaceDiagnosticReport { items: diagnostics },
        ))
    }

    async fn diagnostic(
        &self,
        params: DocumentDiagnosticParams,
    ) -> jsonrpc::Result<DocumentDiagnosticReportResult> {
        let diagnostics = self.diagnostics_vec(Some(&params.text_document.uri)).await;
        self.client
            .log_message(
                MessageType::INFO,
                format!(
                    "updating {} document diagnostic for {}",
                    diagnostics.len(),
                    params.text_document.uri
                ),
            )
            .await;
        self.client
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

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        // clear diagnostics to avoid a stale diagnostics flash on open
        // if the file has typos fixed outside of vscode
        // see https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#textDocument_publishDiagnostics
        self.client
            .publish_diagnostics(params.text_document.uri, Vec::new(), None)
            .await;
    }

    async fn shutdown(&self) -> jsonrpc::Result<()> {
        self.client
            .log_message(MessageType::INFO, format!("{PKG_NAME} server stopped!"))
            .await;
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

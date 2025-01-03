//! Bacon Language Server
use std::env;
use std::path::Path;
use std::time::Duration;

use argh::FromArgs;
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::RwLock;
use tower_lsp::{
    lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range, Url, WorkspaceFolder},
    Client, LspService, Server,
};
use tracing_subscriber::fmt::format::FmtSpan;

mod lsp;

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

#[derive(Debug, Default)]
pub struct BaconLs {
    client: Option<Client>,
    state: RwLock<State>,
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
                                Self::parse_bacon_diagnostic_line(&line, folder_path, uri)
                            {
                                if !diagnostics.iter().any(
                                    |(existing_path, existing_diagnostic)| {
                                        existing_path.path() == path.path()
                                            && diagnostic.range == existing_diagnostic.range
                                            && diagnostic.severity == existing_diagnostic.severity
                                            && diagnostic.message == existing_diagnostic.message
                                    },
                                ) {
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

    async fn publish_diagnostics(&self, uri: &Url) {
        if let Some(client) = self.client.as_ref() {
            client
                .publish_diagnostics(uri.clone(), self.diagnostics_vec(Some(uri)).await, None)
                .await;
        }
    }

    fn parse_severity(severity_str: &str) -> DiagnosticSeverity {
        match severity_str {
            "warning" => DiagnosticSeverity::WARNING,
            "info" | "information" | "note" => DiagnosticSeverity::INFORMATION,
            "hint" | "help" => DiagnosticSeverity::HINT,
            _ => DiagnosticSeverity::ERROR,
        }
    }

    fn parse_positions(fields: &[&str]) -> Option<(u32, u32, u32, u32)> {
        let line_start = fields.first()?.parse().ok()?;
        let line_end = fields.get(1)?.parse().ok()?;
        let column_start = fields.get(2)?.parse().ok()?;
        let column_end = fields.get(3)?.parse().ok()?;
        Some((line_start, line_end, column_start, column_end))
    }

    fn remove_none_suffix(s: &mut String) {
        let suffix = " none";
        if s.ends_with(suffix) {
            // Calculate the position where the suffix starts
            let new_len = s.len() - suffix.len();
            // Truncate the string to this new length, effectively removing the suffix
            s.truncate(new_len);
        }
    }

    fn parse_bacon_diagnostic_line(
        line: &str,
        folder_path: &Path,
        uri: Option<&Url>,
    ) -> Option<(Url, Diagnostic)> {
        // Split line into parts; expect exactly 7 parts in the format specified.
        let line_split: Vec<_> = line.splitn(7, ':').collect();

        if line_split.len() != 7 {
            tracing::error!(
                "malformed line: expected 7 parts in the format of `severity:path:line_start:line_end:column_start:column_end:message` but found {}: {}",
                line_split.len(),
                line
            );
            return None;
        }

        // Parse elements from the split line
        let severity = Self::parse_severity(line_split[0]);
        let file_path = folder_path.join(line_split[1]);

        // Handle potential parse errors
        let (line_start, line_end, column_start, column_end) =
            match Self::parse_positions(&line_split[2..6]) {
                Some(values) => values,
                None => {
                    tracing::error!("error parsing diagnostic position {:?}", &line_split[2..6]);
                    return None;
                }
            };

        let path = match Url::parse(&format!("file://{}", file_path.display())) {
            Ok(url) => url,
            Err(e) => {
                tracing::error!("error parsing file path {}: {}", file_path.display(), e);
                return None;
            }
        };

        // Check URI match
        if uri.is_some() && Some(&path) != uri {
            tracing::debug!("URI mismatch: expected {:?}, found {:?}", uri, path);
            return None;
        }

        let mut message = line_split[6].replace("\\n", "\n");
        Self::remove_none_suffix(&mut message);

        tracing::debug!(
            "new diagnostic: severity: {severity:?}, path: {path:?}, line_start: {line_start}, line_end: {line_end}, column_start: {column_start}, column_end: {column_end}, message: {message}",
        );

        // Create the Diagnostic object
        let diagnostic = Diagnostic {
            range: Range::new(
                Position::new(line_start - 1, column_start - 1),
                Position::new(line_end - 1, column_end - 1),
            ),
            severity: Some(severity),
            source: Some(PKG_NAME.to_string()),
            message,
            ..Diagnostic::default()
        };

        Some((path, diagnostic))
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::str::FromStr;

    use super::*;
    use pretty_assertions::assert_eq;
    use tempdir::TempDir;

    const ERROR_LINE: &str = "error:/app/github/bacon-ls/src/lib.rs:352:352:9:20:cannot find value `one` in this scope\n    |\n352 |         one\n    |         ^^^ help: a unit variant with a similar name exists: `None`\n    |\n   ::: /Users/matteobigoi/.rustup/toolchains/stable-aarch64-apple-darwin/lib/rustlib/src/rust/library/core/src/option.rs:576:5\n    |\n576 |     None,\n    |     ---- similarly named unit variant `None` defined here\n\nFor more information about this error, try `rustc --explain E0425`.\nerror: could not compile `bacon-ls` (lib) due to 1 previous error";

    #[test]
    fn test_parse_bacon_diagnostic_line_with_spans_ok() {
        let result = BaconLs::parse_bacon_diagnostic_line(
            ERROR_LINE,
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
        let result = BaconLs::parse_bacon_diagnostic_line(
            ERROR_LINE,
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
        let result = BaconLs::parse_bacon_diagnostic_line(
            ERROR_LINE,
            Path::new("/app/github/bacon-ls"),
            Some(&Url::from_str("file:///app/github/bacon-ls/src/main.rs").unwrap()),
        );
        assert_eq!(result, None);

        // Non parsable path
        let result = BaconLs::parse_bacon_diagnostic_line(
            ERROR_LINE,
            Path::new("/app/github/bacon-ls"),
            Some(&Url::from_str("fle://app/github/bacon-ls/src/main.rs").unwrap()),
        );
        assert_eq!(result, None);

        // Unparsable line
        let result = BaconLs::parse_bacon_diagnostic_line(
            "warning:/file:1:1",
            Path::new("/app/github/bacon-ls"),
            Some(&Url::from_str("fle://app/github/bacon-ls/src/main.rs").unwrap()),
        );
        assert_eq!(result, None);

        // Empty line
        let result = BaconLs::parse_bacon_diagnostic_line(
            "",
            Path::new("/app/github/bacon-ls"),
            Some(&Url::from_str("fle://app/github/bacon-ls/src/main.rs").unwrap()),
        );
        assert_eq!(result, None);
    }

    // TODO: I need a windows machine to understand why this test fails. I am pretty sure it's
    // because of how the Url is handled in Windows compared to *NIX, but until I don't have a
    // proper test bed Windows support is probably broken.
    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn test_diagnostics_production_and_deduplication() {
        let tmp_dir = TempDir::new("bacon-ls").unwrap();
        let file_path = tmp_dir.path().join(".bacon-locations");
        let mut tmp_file = std::fs::File::create(file_path).unwrap();
        let error_path = format!("{}/src/lib.rs", tmp_dir.path().display());
        let error_path_url = Url::from_str(&format!("file://{error_path}")).unwrap();
        writeln!(
            tmp_file,
            "error:{error_path}:352:352:9:20:cannot find value `one` in this scope"
        )
        .unwrap();
        // duplicate the line
        writeln!(
            tmp_file,
            "error:{error_path}:352:352:9:20:cannot find value `one` in this scope"
        )
        .unwrap();
        writeln!(
            tmp_file,
            "warning:{error_path}:354:354:9:20:cannot find value `two` in this scope"
        )
        .unwrap();
        writeln!(
            tmp_file,
            "help:{error_path}:356:356:9:20:cannot find value `three` in this scope"
        )
        .unwrap();

        let bacon_ls = BaconLs::default();
        let mut state = bacon_ls.state.write().await;
        state.workspace_folders = Some(vec![WorkspaceFolder {
            name: tmp_dir.path().display().to_string(),
            uri: Url::from_directory_path(tmp_dir.path()).unwrap(),
        }]);
        drop(state);
        let diagnostics = bacon_ls.diagnostics(Some(&error_path_url)).await;
        assert_eq!(diagnostics.len(), 3);
    }
}

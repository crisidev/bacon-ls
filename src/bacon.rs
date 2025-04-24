use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use std::{env, fs};

use notify_debouncer_full::{DebounceEventResult, new_debouncer};
use serde::{Deserialize, Serialize};
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tower_lsp::Client;
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range, Url, WorkspaceFolder};

use crate::{BaconLs, DiagnosticData, LOCATIONS_FILE, PKG_NAME, State};

#[derive(Debug, Deserialize, Serialize)]
struct BaconConfig {
    jobs: Jobs,
    exports: Exports,
}

#[derive(Debug, Deserialize, Serialize)]
struct Jobs {
    #[serde(rename = "bacon-ls")]
    bacon_ls: BaconLsJob,
}

#[derive(Debug, Deserialize, Serialize)]
struct BaconLsJob {
    #[serde(skip_deserializing)]
    command: Vec<String>,
    analyzer: String,
    need_stdout: bool,
}

#[derive(Debug, Deserialize, Serialize)]
struct Exports {
    #[serde(rename = "cargo-json-spans")]
    cargo_json_spans: CargoJsonSpans,
}

#[derive(Debug, Deserialize, Serialize)]
struct CargoJsonSpans {
    auto: bool,
    exporter: String,
    line_format: String,
    path: String,
}

const ERROR_MESSAGE: &str = "bacon configuration is not compatible with bacon-ls: please take a look to https://github.com/crisidev/bacon-ls?tab=readme-ov-file#configuration and adapt your bacon configuration";
const BACON_ANALYZER: &str = "cargo_json";
const BACON_EXPORTER: &str = "analyzer";
const BACON_COMMAND: [&str; 7] = [
    "cargo",
    "clippy",
    "--tests",
    "--all-targets",
    "--all-features",
    "--message-format",
    "json-diagnostic-rendered-ansi",
];
const LINE_FORMAT: &str = "{diagnostic.level}|:|{span.file_name}|:|{span.line_start}|:|{span.line_end}|:|{span.column_start}|:|{span.column_end}|:|{diagnostic.message}|:|{diagnostic.rendered}|:|{span.suggested_replacement}";

pub(crate) struct Bacon;

impl Bacon {
    async fn validate_preferences_file(path: &Path) -> Result<(), String> {
        let toml_content = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| format!("{ERROR_MESSAGE}: {e}"))?;
        let config: BaconConfig = toml::from_str(&toml_content).map_err(|e| format!("{ERROR_MESSAGE}: {e}"))?;
        tracing::debug!("bacon config is {config:#?}");
        if config.jobs.bacon_ls.analyzer == BACON_ANALYZER
            && config.jobs.bacon_ls.need_stdout
            && config.exports.cargo_json_spans.auto
            && config.exports.cargo_json_spans.exporter == BACON_EXPORTER
            && config.exports.cargo_json_spans.line_format == LINE_FORMAT
            && config.exports.cargo_json_spans.path == LOCATIONS_FILE
        {
            tracing::info!("bacon configuration {} is valid", path.display());
            Ok(())
        } else {
            Err(ERROR_MESSAGE.to_string())
        }
    }

    async fn create_preferences_file(filename: &str) -> Result<(), String> {
        let bacon_config = BaconConfig {
            jobs: Jobs {
                bacon_ls: BaconLsJob {
                    command: BACON_COMMAND.map(|c| c.to_string()).into_iter().collect(),
                    analyzer: BACON_ANALYZER.to_string(),
                    need_stdout: true,
                },
            },
            exports: Exports {
                cargo_json_spans: CargoJsonSpans {
                    auto: true,
                    exporter: BACON_EXPORTER.to_string(),
                    line_format: LINE_FORMAT.to_string(),
                    path: LOCATIONS_FILE.to_string(),
                },
            },
        };
        tracing::info!("creating new bacon preference file {filename}",);
        let toml_string = toml::to_string_pretty(&bacon_config)
            .map_err(|e| format!("error serializing bacon preferences {filename} content: {e}"))?;
        let mut file = File::create(filename)
            .await
            .map_err(|e| format!("error creating bacon preferences {filename}: {e}"))?;
        file.write_all(toml_string.as_bytes())
            .await
            .map_err(|e| format!("error writing bacon preferences {filename}: {e}"))?;
        Ok(())
    }

    async fn validate_preferences_impl(bacon_prefs: &[u8], create_prefs_file: bool) -> Result<(), String> {
        let bacon_prefs_files = String::from_utf8_lossy(bacon_prefs);
        let bacon_prefs_files_split: Vec<&str> = bacon_prefs_files.split("\n").collect();
        let mut preference_file_exists = false;
        for prefs_file in bacon_prefs_files_split.iter() {
            let prefs_file_path = Path::new(prefs_file);
            if prefs_file_path.exists() {
                preference_file_exists = true;
                Self::validate_preferences_file(prefs_file_path).await?;
            } else {
                tracing::debug!("skipping non existing bacon preference file {prefs_file}");
            }
        }

        if !preference_file_exists && create_prefs_file {
            Self::create_preferences_file(bacon_prefs_files_split[0]).await?;
        }

        Ok(())
    }

    pub(crate) fn find_bacon_locations(
        root: &Path,
        locations_file_name: &str,
        results: &mut Vec<PathBuf>,
    ) -> Result<(), Box<dyn Error>> {
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            let path = entry.path();

            if path.is_dir() {
                Self::find_bacon_locations(&path, locations_file_name, results)?;
            } else if path.file_name().is_some_and(|name| name == locations_file_name) {
                results.push(path);
            }
        }
        Ok(())
    }

    fn parse_severity(severity_str: &str) -> DiagnosticSeverity {
        match severity_str {
            "warning" => DiagnosticSeverity::WARNING,
            "info" | "information" | "note" | "failure-note" => DiagnosticSeverity::INFORMATION,
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

    fn parse_bacon_diagnostic_line(line: &str, folder_path: &Path) -> Option<(Url, Diagnostic)> {
        // Split line into parts; expect exactly 7 parts in the format specified.
        let line_split: Vec<_> = line.splitn(9, "|:|").collect();

        if line_split.len() != 9 {
            tracing::error!(
                "malformed line: expected 8 parts in the format of `severity|:|path|:|line_start|:|line_end|:|column_start|:|column_end|:|message|:|rendered_message|:|replacement` but found {}: {}",
                line_split.len(),
                line
            );
            return None;
        }

        // Parse elements from the split line
        let severity = Self::parse_severity(line_split[0]);
        let file_path = folder_path.join(line_split[1]);

        // Handle potential parse errors
        let (line_start, line_end, column_start, column_end) = match Self::parse_positions(&line_split[2..6]) {
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

        let mut message = line_split[6].replace("\\n", "\n").trim_end_matches('\n').to_string();
        let replacement = line_split[8];
        let data = if replacement != "none" {
            tracing::debug!("storing potential quick fix code action to replace word with {replacement}");
            Some(serde_json::json!(DiagnosticData {
                corrections: vec![replacement.into()]
            }))
        } else {
            None
        };

        tracing::debug!(
            "new diagnostic: severity: {severity:?}, path: {path:?}, line_start: {line_start}, line_end: {line_end}, column_start: {column_start}, column_end: {column_end}, message: {message}",
        );

        // Create the Diagnostic object
        let rendered_message = line_split[7];
        if rendered_message != "none" {
            message = ansi_regex::ansi_regex()
                .replace_all(rendered_message, "")
                .trim_end_matches('\n')
                .to_string()
        }
        let diagnostic = Diagnostic {
            range: Range::new(
                Position::new(line_start - 1, column_start - 1),
                Position::new(line_end - 1, column_end - 1),
            ),
            severity: Some(severity),
            source: Some(PKG_NAME.to_string()),
            message,
            data,
            ..Diagnostic::default()
        };

        Some((path, diagnostic))
    }

    fn deduplicate_diagnostics(path: Url, uri: &Url, diagnostic: Diagnostic, diagnostics: &mut Vec<(Url, Diagnostic)>) {
        if &path == uri
            && !diagnostics.iter().any(|(existing_path, existing_diagnostic)| {
                existing_path.path() == path.path()
                    && diagnostic.range == existing_diagnostic.range
                    && diagnostic.severity == existing_diagnostic.severity
                    && diagnostic.message == existing_diagnostic.message
            })
        {
            diagnostics.push((path, diagnostic));
        }
    }

    pub(crate) async fn validate_preferences(create_prefs_file: bool) -> Result<(), String> {
        let bacon_prefs = Command::new("bacon")
            .arg("--prefs")
            .output()
            .await
            .map_err(|e| e.to_string())?;
        Self::validate_preferences_impl(&bacon_prefs.stdout, create_prefs_file).await
    }

    pub(crate) async fn run_in_background(
        bacon_command: &str,
        bacon_command_args: &str,
        current_dir: Option<&PathBuf>,
        cancel_token: CancellationToken,
    ) -> Result<JoinHandle<()>, String> {
        tracing::info!("starting bacon in background with arguments `{bacon_command_args}`");
        let log_bacon = env::var("BACON_LS_LOG_BACON").unwrap_or("on".to_string());
        let mut command = Command::new(bacon_command);
        command
            .args(bacon_command_args.split_whitespace().collect::<Vec<&str>>())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(current_dir) = current_dir {
            command.current_dir(current_dir);
        }

        match command.spawn() {
            Ok(mut child) => {
                // Handle stdout
                if log_bacon != "off" {
                    if let Some(stdout) = child.stdout.take() {
                        let reader = BufReader::new(stdout).lines();
                        tokio::spawn(async move {
                            let mut reader = reader;
                            while let Ok(Some(line)) = reader.next_line().await {
                                tracing::info!("[bacon stdout]: {}", line);
                            }
                        });
                    }
                }

                // Handle stderr
                if log_bacon != "off" {
                    if let Some(stderr) = child.stderr.take() {
                        let reader = BufReader::new(stderr).lines();
                        tokio::spawn(async move {
                            let mut reader = reader;
                            while let Ok(Some(line)) = reader.next_line().await {
                                tracing::error!("[bacon stderr]: {}", line);
                            }
                        });
                    }
                }

                // Wait for the child process to finish
                Ok(tokio::spawn(async move {
                    tracing::debug!("waiting for bacon to terminate");
                    tokio::select! {
                        _ = child.wait() => {},
                        _ = cancel_token.cancelled() => {},
                    };
                }))
            }
            Err(e) => Err(format!("failed to start bacon: {e}")),
        }
    }

    async fn diagnostics(
        uri: &Url,
        locations_file_name: &str,
        workspace_folders: Option<&[WorkspaceFolder]>,
    ) -> Vec<(Url, Diagnostic)> {
        let mut diagnostics: Vec<(Url, Diagnostic)> = vec![];

        if let Some(workspace_folders) = workspace_folders {
            for folder in workspace_folders.iter() {
                let mut folder_path = folder
                    .uri
                    .to_file_path()
                    .expect("the workspace folder sent by the editor is not a file path");
                if let Some(git_root) = BaconLs::find_git_root_directory(&folder_path).await {
                    tracing::debug!(
                        "found git root directory {}, using it for files base path",
                        git_root.display()
                    );
                    folder_path = git_root;
                }
                let mut bacon_locations = Vec::new();
                if let Err(e) = Bacon::find_bacon_locations(&folder_path, locations_file_name, &mut bacon_locations) {
                    tracing::warn!("unable to find valid bacon loctions files: {e}");
                }
                for bacon_location in bacon_locations.iter() {
                    tracing::info!("found bacon locations file to parse {}", bacon_location.display());
                    match File::open(&bacon_location).await {
                        Ok(fd) => {
                            let reader = BufReader::new(fd);
                            let mut lines = reader.lines();
                            let mut buffer = String::new();

                            while let Some(line) = lines.next_line().await.unwrap_or_else(|e| {
                                tracing::error!("error reading line from file {}: {e}", bacon_location.display());
                                None
                            }) {
                                let trimmed = line.trim_end();

                                // Use the first word to determine the start of a new diagnostic
                                let is_new_diagnostic = trimmed.starts_with("warning")
                                    || trimmed.starts_with("error")
                                    || trimmed.starts_with("info")
                                    || trimmed.starts_with("note")
                                    || trimmed.starts_with("failure-note")
                                    || trimmed.starts_with("help");

                                if is_new_diagnostic {
                                    // Process the collected buffer before starting a new entry
                                    if !buffer.is_empty() {
                                        if let Some((path, diagnostic)) =
                                            Self::parse_bacon_diagnostic_line(&buffer, &folder_path)
                                        {
                                            tracing::debug!("found diagnostic for {}", path);
                                            Self::deduplicate_diagnostics(
                                                path.clone(),
                                                uri,
                                                diagnostic,
                                                &mut diagnostics,
                                            );
                                        }
                                    }
                                    // Reset buffer for new diagnostic entry
                                    buffer.clear();
                                }

                                // Append current line to buffer
                                if !buffer.is_empty() {
                                    buffer.push('\n'); // Preserve multiline structure
                                }
                                buffer.push_str(trimmed);
                            }

                            // Flush the remaining buffer after loop ends
                            if !buffer.is_empty() {
                                if let Some((path, diagnostic)) =
                                    Self::parse_bacon_diagnostic_line(&buffer, &folder_path)
                                {
                                    Self::deduplicate_diagnostics(path.clone(), uri, diagnostic, &mut diagnostics);
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!("unable to read file {}: {e}", bacon_location.display())
                        }
                    }
                }
            }
        }
        diagnostics
    }

    async fn diagnostics_vec(
        uri: &Url,
        locations_file_name: &str,
        workspace_folders: Option<&[WorkspaceFolder]>,
    ) -> Vec<Diagnostic> {
        Self::diagnostics(uri, locations_file_name, workspace_folders)
            .await
            .into_iter()
            .map(|(_, y)| y)
            .collect::<Vec<Diagnostic>>()
    }

    pub(crate) async fn syncronize_diagnostics(state: Arc<RwLock<State>>, client: Option<Arc<Client>>) {
        tracing::info!("starting background task in charge of syncronizing diagnostics for all open files");
        let (tx, rx) = flume::unbounded::<DebounceEventResult>();

        let (locations_file, wait_time, cancel_token) = {
            let state = state.read().await;
            (
                state.locations_file.clone(),
                state.syncronize_all_open_files_wait_millis,
                state.cancel_token.clone(),
            )
        };

        let mut watcher = new_debouncer(wait_time, None, move |ev: DebounceEventResult| {
            // Returns an error if all senders are dropped.
            let _res = tx.send(ev);
        })
        .expect("failed to create file watcher");

        loop {
            match watcher.watch(PathBuf::from(&locations_file), notify::RecursiveMode::Recursive) {
                Ok(_) => break,
                Err(e) => {
                    tracing::info!("unable to start .bacon_locations file watcher, retrying in 1 second");
                    tracing::debug!(".bacon_locations watcher: {e}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }

        while let Some(Ok(res)) = tokio::select! {
            ev = rx.recv_async() => {
                Some(ev)
            }
            _ = cancel_token.cancelled() => {
                None
            }
        } {
            let events = match res {
                Ok(events) => events,
                Err(err) => {
                    tracing::error!(?err, "watch error");
                    continue;
                }
            };
            // Only publish if the file was modified.
            if !events.iter().any(|ev| ev.kind.is_modify()) {
                continue;
            }

            let loop_state = state.read().await;
            let open_files = loop_state.open_files.clone();
            let locations_file = loop_state.locations_file.clone();
            let workspace_folders = loop_state.workspace_folders.clone();
            drop(loop_state);
            tracing::debug!(
                "running periodic diagnostic publish for open files `{}`",
                open_files.iter().map(|f| f.to_string()).collect::<Vec<_>>().join(",")
            );
            for uri in open_files.iter() {
                Self::publish_diagnostics(client.as_ref(), uri, &locations_file, workspace_folders.as_deref()).await;
            }
        }
    }

    pub(crate) async fn publish_diagnostics(
        client: Option<&Arc<Client>>,
        uri: &Url,
        locations_file_name: &str,
        workspace_folders: Option<&[WorkspaceFolder]>,
    ) {
        let diagnostics_vec = Self::diagnostics_vec(uri, locations_file_name, workspace_folders).await;
        tracing::info!("sent {} bacon diagnostics for {uri}", diagnostics_vec.len());
        if let Some(client) = client {
            client.publish_diagnostics(uri.clone(), diagnostics_vec, None).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{io::Write, str::FromStr};

    use super::*;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_valid_bacon_preferences() {
        let valid_toml = format!(
            r#"
            [jobs.bacon-ls]
            analyzer = "{BACON_ANALYZER}"
            need_stdout = true

            [exports.cargo-json-spans]
            auto = true
            exporter = "{BACON_EXPORTER}"
            line_format = "{LINE_FORMAT}"
            path = "{LOCATIONS_FILE}"
        "#
        );
        let tmp_dir = TempDir::new().unwrap();
        let file_path = tmp_dir.path().join("prefs.toml");
        let mut file = std::fs::File::create(&file_path).unwrap();
        write!(file, "{}", valid_toml).unwrap();
        assert!(Bacon::validate_preferences_file(&file_path).await.is_ok());
    }

    #[tokio::test]
    async fn test_invalid_analyzer() {
        let invalid_toml = format!(
            r#"
            [jobs.bacon-ls]
            analyzer = "incorrect_analyzer"
            need_stdout = true

            [exports.cargo-json-spans]
            auto = true
            exporter = "{BACON_EXPORTER}"
            line_format = "{LINE_FORMAT}"
            path = "{LOCATIONS_FILE}"
        "#
        );

        let tmp_dir = TempDir::new().unwrap();
        let file_path = tmp_dir.path().join("prefs.toml");
        let mut file = std::fs::File::create(&file_path).unwrap();
        write!(file, "{}", invalid_toml).unwrap();
        assert!(Bacon::validate_preferences_file(&file_path).await.is_err());
    }

    #[tokio::test]
    async fn test_invalid_line_format() {
        let invalid_toml = format!(
            r#"
            [jobs.bacon-ls]
            analyzer = "{BACON_ANALYZER}"
            need_stdout = true

            [exports.cargo-json-spans]
            auto = true
            exporter = "{BACON_EXPORTER}"
            line_format = "invalid_line_format"
            path = "{LOCATIONS_FILE}"
        "#
        );

        let tmp_dir = TempDir::new().unwrap();
        let file_path = tmp_dir.path().join("prefs.toml");
        let mut file = std::fs::File::create(&file_path).unwrap();
        write!(file, "{}", invalid_toml).unwrap();
        assert!(Bacon::validate_preferences_file(&file_path).await.is_err());
    }

    #[tokio::test]
    async fn test_validate_preferences() {
        let valid_toml = format!(
            r#"
            [jobs.bacon-ls]
            analyzer = "{BACON_ANALYZER}"
            need_stdout = true

            [exports.cargo-json-spans]
            auto = true
            exporter = "{BACON_EXPORTER}"
            line_format = "{LINE_FORMAT}"
            path = "{LOCATIONS_FILE}"
        "#
        );
        assert!(
            Bacon::validate_preferences_impl(valid_toml.as_bytes(), false)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn test_file_creation_failure() {
        let invalid_path = "/invalid/path/to/file.toml";
        let result = Bacon::create_preferences_file(invalid_path).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("error creating bacon preferences"));
    }

    #[tokio::test]
    async fn test_file_write_failure() {
        let tmp_dir = TempDir::new().unwrap();
        let file_path = tmp_dir.path().join("prefs.toml");
        // Simulate write failure by closing the file prematurely
        let file = File::create(&file_path).await.unwrap();
        drop(file); // Close the file to simulate failure
        let result = Bacon::create_preferences_file(file_path.to_str().unwrap()).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_empty_bacon_preferences_file() {
        let tmp_dir = TempDir::new().unwrap();
        let file_path = tmp_dir.path().join("empty_prefs.toml");
        std::fs::File::create(&file_path).unwrap();
        assert!(Bacon::validate_preferences_file(&file_path).await.is_err());
    }

    #[tokio::test]
    async fn test_run_in_background() {
        let cancel_token = CancellationToken::new();
        let handle = Bacon::run_in_background("cargo", "--version", None, cancel_token.clone()).await;
        assert!(handle.is_ok());
        cancel_token.cancel();
        handle.unwrap().await.unwrap();
    }

    const ERROR_LINE: &str = "error|:|/app/github/bacon-ls/src/lib.rs|:|352|:|352|:|9|:|20|:|cannot find value `one` in this scope\n    |\n352 |         one\n    |         ^^^ help: a unit variant with a similar name exists: `None`\n    |\n   ::: /Users/matteobigoi/.rustup/toolchains/stable-aarch64-apple-darwin/lib/rustlib/src/rust/library/core/src/option.rs:576:5\n    |\n576 |     None,\n    |     ---- similarly named unit variant `None` defined here\n\nFor more information about this error, try `rustc --explain E0425`.\nerror: could not compile `bacon-ls` (lib) due to 1 previous error|:|none|:|none";

    #[test]
    fn test_parse_bacon_diagnostic_line_with_spans_ok() {
        let result = Bacon::parse_bacon_diagnostic_line(ERROR_LINE, Path::new("/app/github/bacon-ls"));
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
        let result = Bacon::parse_bacon_diagnostic_line(ERROR_LINE, Path::new("/app/github/bacon-ls"));
        let (url, diagnostic) = result.unwrap();
        assert_eq!(url.to_string(), "file:///app/github/bacon-ls/src/lib.rs");
        assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(diagnostic.source, Some(PKG_NAME.to_string()));
    }

    #[test]
    fn test_parse_bacon_diagnostic_line_with_spans_ko() {
        // Unparsable line
        let result = Bacon::parse_bacon_diagnostic_line("warning:/file:1:1", Path::new("/app/github/bacon-ls"));
        assert_eq!(result, None);

        // Empty line
        let result = Bacon::parse_bacon_diagnostic_line("", Path::new("/app/github/bacon-ls"));
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn test_bacon_multiline_diagnostics_production() {
        let tmp_dir = TempDir::new().unwrap();
        let file_path = tmp_dir.path().join(".bacon-locations");
        let mut tmp_file = std::fs::File::create(file_path).unwrap();
        let error_path = format!("{}/src/lib.rs", tmp_dir.path().display());
        let error_path_url = Url::from_str(&format!("file://{error_path}")).unwrap();
        writeln!(
            tmp_file,
            "warning|:|src/lib.rs|:|130|:|142|:|33|:|34|:|this if statement can be collapsed|:|none|:|none"
        )
        .unwrap();
        writeln!(
            tmp_file,
            r#"help|:|{error_path}|:|130|:|142|:|33|:|34|:|collapse nested if block|:|none|:|if Some(&the_path) == uri && !diagnostics.iter().any(
                                        |(existing_path, existing_diagnostic)| {{
                                            existing_path.path() == the_path.path()
                                                && diagnostic.range == existing_diagnostic.range
                                                && diagnostic.severity
                                                    == existing_diagnostic.severity
                                                && diagnostic.message == existing_diagnostic.message
                                        }},
                                    ) {{
                                    diagnostics.push((path, diagnostic));
                                }}"#
        ).unwrap();
        writeln!(
            tmp_file,
            "warning|:|{error_path}|:|150|:|162|:|33|:|34|:|this if statement can be collapsed again|:|none|:|none"
        )
        .unwrap();
        writeln!(
            tmp_file,
            r#"warning|:|{error_path}|:|150|:|162|:|33|:|34|:|collapse nested if block|:|if Some(&other_path) == uri && !diagnostics.iter().any(
                                        |(existing_path, existing_diagnostic)| {{
                                            existing_path.path() == other_path.path()
                                                && diagnostic.range == existing_diagnostic.range
                                                && diagnostic.severity
                                                    == existing_diagnostic.severity
                                                && diagnostic.message == existing_diagnostic.message
                                        }},
                                    ) {{
                                    diagnostics.push((path, diagnostic));
                                }}|:|none"#
        ).unwrap();

        let workspace_folders = Some(vec![WorkspaceFolder {
            name: tmp_dir.path().display().to_string(),
            uri: Url::from_directory_path(tmp_dir.path()).unwrap(),
        }]);
        let diagnostics = Bacon::diagnostics(&error_path_url, LOCATIONS_FILE, workspace_folders.as_deref()).await;
        assert_eq!(diagnostics.len(), 4);
        assert!(diagnostics[0].1.data.is_none());
        assert_eq!(diagnostics[0].1.message.len(), 34);
        assert!(diagnostics[1].1.data.is_some());
        assert_eq!(diagnostics[1].1.message.len(), 24);
        assert!(diagnostics[2].1.data.is_none());
        assert_eq!(diagnostics[2].1.message.len(), 40);
        assert!(diagnostics[3].1.data.is_none());
        assert_eq!(diagnostics[3].1.message.len(), 766);
    }

    #[tokio::test]
    async fn test_bacon_diagnostics_production_and_deduplication() {
        let tmp_dir = TempDir::new().unwrap();
        let file_path = tmp_dir.path().join(".bacon-locations");
        let mut tmp_file = std::fs::File::create(file_path).unwrap();
        let error_path = format!("{}/src/lib.rs", tmp_dir.path().display());
        let error_path_url = Url::from_str(&format!("file://{error_path}")).unwrap();
        writeln!(
            tmp_file,
            "error|:|{error_path}|:|352|:|352|:|9|:|20|:|cannot find value `one` in this scope|:|none|:|none"
        )
        .unwrap();
        // duplicate the line
        writeln!(
            tmp_file,
            "error|:|{error_path}|:|352|:|352|:|9|:|20|:|cannot find value `one` in this scope|:|none|:|none"
        )
        .unwrap();
        writeln!(
            tmp_file,
            "warning|:|{error_path}|:|354|:|354|:|9|:|20|:|cannot find value `two` in this scope|:|some|:|none"
        )
        .unwrap();
        writeln!(
            tmp_file,
            "help|:|{error_path}|:|356|:|356|:|9|:|20|:|cannot find value `three` in this scope|:|none|:|some other"
        )
        .unwrap();

        let workspace_folders = Some(vec![WorkspaceFolder {
            name: tmp_dir.path().display().to_string(),
            uri: Url::from_directory_path(tmp_dir.path()).unwrap(),
        }]);
        let diagnostics = Bacon::diagnostics(&error_path_url, LOCATIONS_FILE, workspace_folders.as_deref()).await;
        assert_eq!(diagnostics.len(), 3);
        let diagnostics_vec =
            Bacon::diagnostics_vec(&error_path_url, LOCATIONS_FILE, workspace_folders.as_deref()).await;
        assert_eq!(diagnostics_vec.len(), 3);
    }
}

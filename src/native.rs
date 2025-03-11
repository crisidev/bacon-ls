use std::{
    collections::HashMap,
    env,
    error::Error,
    io,
    path::{Path, PathBuf},
    process::Stdio,
};

use serde::{Deserialize, Deserializer};
use tokio::{fs, process::Command};
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range, Url};

use crate::{DiagnosticData, PKG_NAME};

#[derive(Debug, Deserialize, Clone, Copy)]
enum CargoLevel {
    #[serde(rename = "error")]
    Error,
    #[serde(rename = "warning")]
    Warning,
    #[serde(rename = "note")]
    Note,
    #[serde(rename = "failure-note")]
    FailureNote,
    #[serde(rename = "help")]
    Help,
}

#[derive(Debug, Deserialize)]
struct CargoSpan {
    #[serde(deserialize_with = "deserialize_url")]
    file_name: Url,
    line_start: u32,
    line_end: u32,
    column_start: u32,
    column_end: u32,
    suggested_replacement: Option<String>,
}

fn deserialize_url<'de, D>(deserializer: D) -> Result<Url, D::Error>
where
    D: Deserializer<'de>,
{
    let url_str: &str = Deserialize::deserialize(deserializer)?;
    Url::parse(&format!("file://{url_str}")).map_err(serde::de::Error::custom)
}

#[derive(Debug, Deserialize)]
struct CargoChildren {
    message: String,
    level: String,
    spans: Vec<CargoSpan>,
}

#[derive(Debug, Deserialize)]
struct CargoMessage {
    #[serde(rename(deserialize = "$message_type"))]
    message_type: String,
    rendered: String,
    level: String,
    spans: Vec<CargoSpan>,
    children: Vec<CargoChildren>,
}

#[derive(Debug, Deserialize)]
struct CargoLine {
    message: Option<CargoMessage>,
}

#[derive(Debug, Default)]
pub(crate) struct Cargo;

impl Cargo {
    fn parse_severity(severity_str: &str) -> DiagnosticSeverity {
        match severity_str {
            "warning" | "failure-note" => DiagnosticSeverity::WARNING,
            "info" | "information" | "note" => DiagnosticSeverity::INFORMATION,
            "hint" | "help" => DiagnosticSeverity::HINT,
            _ => DiagnosticSeverity::ERROR,
        }
    }

    fn maybe_add_diagnostic(
        current_dir: &Path,
        severity: &str,
        message: &str,
        span: &CargoSpan,
        diagnostics: &mut HashMap<Url, Vec<Diagnostic>>,
    ) -> Result<(), Box<dyn Error>> {
        if let Some(host) = span.file_name.host().as_ref() {
            let data = span.suggested_replacement.as_ref().map(|replacement| {
                serde_json::json!(DiagnosticData {
                    corrections: vec![replacement.into()]
                })
            });
            let file_name = current_dir
                .join(host.to_string())
                .join(span.file_name.path().replacen("/", "", 1));
            let url = Url::parse(&format!("file://{}", file_name.display()))?;
            let diagnostics: &mut Vec<Diagnostic> = diagnostics.entry(url).or_default();
            let diagnostic = Diagnostic {
                range: Range::new(
                    Position::new(span.line_start - 1, span.column_start - 1),
                    Position::new(span.line_end - 1, span.column_end - 1),
                ),
                severity: Some(Self::parse_severity(severity)),
                source: Some(PKG_NAME.to_string()),
                message: message.to_string(),
                data,
                ..Diagnostic::default()
            };
            if !diagnostics.iter().any(|existing_diagnostic| {
                diagnostic.range == existing_diagnostic.range
                    && diagnostic.severity == existing_diagnostic.severity
                    && diagnostic.message == existing_diagnostic.message
            }) {
                diagnostics.push(diagnostic);
            }
        }
        Ok(())
    }

    pub(crate) async fn cargo_diagnostics(
        command_args: &str,
        destination_folder: &Path,
    ) -> Result<HashMap<Url, Vec<Diagnostic>>, Box<dyn Error>> {
        let mut args: Vec<&str> = command_args.split_whitespace().collect();
        args.push("--manifest-path");
        let cargo_toml = destination_folder.join("Cargo.toml").display().to_string();
        args.push(&cargo_toml);
        tracing::debug!("running command `cargo {args:?}`");
        let child = Command::new("cargo")
            .args(command_args.split_whitespace())
            .current_dir(destination_folder)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output()
            .await?;
        tracing::debug!("cargo command finished with status {}", child.status);

        let stdout = String::from_utf8_lossy(&child.stdout);
        let stderr = String::from_utf8_lossy(&child.stderr);
        let mut diagnostics = HashMap::new();
        let current_dir = env::current_dir()?;

        for line in stdout.lines() {
            match serde_json::from_str::<CargoLine>(line) {
                Ok(message) => {
                    if let Some(message) = message.message {
                        if message.message_type == "diagnostic" {
                            for span in message.spans.into_iter() {
                                Self::maybe_add_diagnostic(
                                    &current_dir,
                                    &message.level,
                                    ansi_regex::ansi_regex()
                                        .replace_all(&message.rendered, "")
                                        .trim_end_matches('\n'),
                                    &span,
                                    &mut diagnostics,
                                )?;
                            }
                            for children in message.children.into_iter() {
                                for span in children.spans.into_iter() {
                                    Self::maybe_add_diagnostic(
                                        &current_dir,
                                        &children.level,
                                        &children.message,
                                        &span,
                                        &mut diagnostics,
                                    )?;
                                }
                            }
                        }
                    }
                }
                Err(e) => tracing::error!("error deserializing cargo line:\n{line}\n{e}"),
            }
        }
        let log_cargo = env::var("BACON_LS_LOG_CARGO").unwrap_or("off".to_string());
        if log_cargo != "off" {
            for line in stderr.lines() {
                tracing::info!("[cargo stderr]{line}");
            }
        }
        Ok(diagnostics)
    }

    pub(crate) async fn find_git_root_directory() -> Option<PathBuf> {
        let output = tokio::process::Command::new("git")
            .arg("rev-parse")
            .arg("--show-toplevel")
            .output()
            .await
            .ok()?;

        if output.status.success() {
            String::from_utf8(output.stdout).ok().map(|v| PathBuf::from(v.trim()))
        } else {
            None
        }
    }

    pub(crate) async fn copy_source_code(destination_folder: &Path) -> Result<(), io::Error> {
        let source_repo = Self::find_git_root_directory()
            .await
            .ok_or(io::Error::new(io::ErrorKind::Other, "oh no!"))?;
        let output = Command::new("git")
            .args(["ls-files"])
            .current_dir(&source_repo)
            .output()
            .await?;

        if !output.status.success() {
            return Err(io::Error::new(io::ErrorKind::Other, "Failed to list tracked files"));
        }

        let files = String::from_utf8_lossy(&output.stdout);
        tracing::debug!(
            "copying all files from {} to {}",
            source_repo.display(),
            destination_folder.display()
        );
        for file in files.lines() {
            let src_path = source_repo.join(file);
            let dest_path = destination_folder.join(file);

            if let Some(parent) = dest_path.parent() {
                fs::create_dir_all(parent).await?; // Ensure the directory exists
            }

            fs::copy(&src_path, &dest_path).await?;
        }

        Ok(())
    }
}

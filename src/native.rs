use std::{collections::HashMap, env, error::Error, path::Path, process::Stdio};

use serde::{Deserialize, Deserializer};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
};
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
    pub(crate) async fn cargo_diagnostics(command_args: &str) -> Result<HashMap<Url, Vec<Diagnostic>>, Box<dyn Error>> {
        let mut child = Command::new("cargo")
            .args(command_args.split_whitespace())
            .stdout(Stdio::piped())
            .spawn()?;

        let stdout = child.stdout.take().ok_or("failed to capture stdout")?;
        let mut reader = BufReader::new(stdout).lines();

        tokio::spawn(async move {
            let status = child.wait().await.expect("child process encountered an error");
            tracing::info!("cargo child command exit status: {status}");
        });

        let mut diagnostics = HashMap::new();
        let current_dir = env::current_dir()?;

        while let Some(line) = reader.next_line().await? {
            match serde_json::from_str::<CargoLine>(&line) {
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
        Ok(diagnostics)
    }
}

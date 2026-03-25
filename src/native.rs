use std::{
    collections::HashMap,
    env,
    path::{Path, PathBuf},
    process::Stdio,
};

use anyhow::Context;
use ls_types::{Diagnostic, DiagnosticSeverity, InitializeParams, Position, Range, Uri};
use serde::{Deserialize, Deserializer};
use tokio::{io::AsyncBufReadExt, process::Command};
use tower_lsp_server::{Bounded, NotCancellable, OngoingProgress};

use crate::{DiagnosticData, PKG_NAME};

#[derive(Debug, Deserialize)]
struct CargoSpan {
    #[serde(deserialize_with = "deserialize_url")]
    file_name: Uri,
    line_start: u32,
    line_end: u32,
    column_start: u32,
    column_end: u32,
    suggested_replacement: Option<String>,
}

fn deserialize_url<'de, D>(deserializer: D) -> Result<Uri, D::Error>
where
    D: Deserializer<'de>,
{
    let url_str: &str = Deserialize::deserialize(deserializer)?;
    str::parse::<Uri>(&format!("file://{url_str}")).map_err(serde::de::Error::custom)
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

/// Parses the ouput line from cargo command that looks like:
/// ```
/// Building [====       ] 1/400: thing, thig2
/// ```
fn parse_building_line(line: &str) -> Option<(String, u32)> {
    if !line.starts_with("Building") {
        return None;
    }
    let after_bracket = line.split("] ").nth(1)?;
    let clean: String = after_bracket.chars().filter(|c| !c.is_control()).collect();

    let (fraction, _crates) = clean.split_once(": ")?;
    let (n_str, total_str) = fraction.split_once('/')?;
    let n: u32 = n_str.parse().ok()?;
    let total: u32 = total_str.parse().ok()?;
    if total == 0 {
        return None;
    }
    let pct = (n * 100 / total).min(99);

    Some((clean, pct))
}

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
        project_root: Option<&PathBuf>,
        severity: &str,
        message: &str,
        span: &CargoSpan,
        diagnostics: &mut HashMap<Uri, Vec<Diagnostic>>,
    ) -> anyhow::Result<()> {
        if let Some(host) = span.file_name.authority().map(|auth| auth.host()) {
            let data = span.suggested_replacement.as_ref().map(|replacement| {
                serde_json::json!(DiagnosticData {
                    corrections: vec![replacement.into()]
                })
            });

            let root_dir = {
                if let Some(project_root) = project_root {
                    project_root
                } else {
                    &env::current_dir().context("getting current dir")?
                }
            };

            let file_name = {
                tracing::debug!(?root_dir, ?host, file_name = ?span.file_name, file_name_str = span.file_name.to_string(), "building uri");
                tracing::debug!("replaced path: {}", span.file_name.path().as_str().replacen("/", "", 1));

                // If host is empty, the span.file_name is an absolute path.
                let uri = if host.is_empty() {
                    PathBuf::from(span.file_name.path().as_str())
                } else {
                    let tmp = root_dir.join(host);
                    // For first level paths, e.g., `build.rs`, this ensures that we dont join an
                    // empty string (because `file_name` is empty), creating a non-existent
                    // `build.rs/` directory in the source root, and therefore failing
                    // canonicalization.
                    if span.file_name.path().as_str().is_empty() {
                        tmp
                    } else {
                        tmp.join(span.file_name.path().as_str().replacen("/", "", 1))
                    }
                };

                // Canonicalization is important, otherwise the file path cannot be compared with the
                // paths we get passed from the LSP server.
                uri.canonicalize()
                    .with_context(|| format!("canonicalizing uri: {}", uri.display()))?
                    .into_os_string()
                    .into_string()
                    .map_err(|_orig| std::io::Error::other("cannot convert file name to string"))
            }?;

            let url = str::parse::<Uri>(&format!("file://{file_name}")).context("parsing filename")?;
            tracing::debug!(uri = url.to_string(), ?host, "maybe adding diagnostic");
            let diagnostics: &mut Vec<Diagnostic> = diagnostics.entry(url.clone()).or_default();
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
                tracing::debug!(uri = url.to_string(), ?diagnostic, "adding diagnostic");
                diagnostics.push(diagnostic);
            }
        }

        Ok(())
    }

    pub(crate) async fn cargo_diagnostics(
        command_args: Vec<String>,
        cargo_env: &[(String, String)],
        project_root: Option<&PathBuf>,
        destination_folder: &Path,
        progress: &OngoingProgress<Bounded, NotCancellable>,
    ) -> anyhow::Result<HashMap<Uri, Vec<Diagnostic>>> {
        tracing::debug!("running command `cargo {command_args:?}`");
        let mut cmd = Command::new("cargo");
        cmd.env("CARGO_TERM_PROGRESS_WHEN", "always");
        cmd.env("CARGO_TERM_PROGRESS_WIDTH", "80");
        for (key, val) in cargo_env {
            cmd.env(key, val);
        }
        let mut child = cmd
            .args(command_args)
            .current_dir(destination_folder)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;

        let stdout = child.stdout.take().context("taking stdout")?;
        let stderr = child.stderr.take().context("taking stderr")?;

        let mut diagnostics = HashMap::new();
        let log_cargo = env::var("BACON_LS_LOG_CARGO").unwrap_or("off".to_string());

        let stdout_future = async {
            let mut reader = tokio::io::BufReader::new(stdout).lines();
            while let Some(line) = reader.next_line().await? {
                match serde_json::from_str::<CargoLine>(&line) {
                    Ok(message) => {
                        if let Some(message) = message.message
                            && message.message_type == "diagnostic"
                        {
                            for span in message.spans.into_iter() {
                                Self::maybe_add_diagnostic(
                                    project_root,
                                    &message.level,
                                    ansi_regex::ansi_regex()
                                        .replace_all(&message.rendered, "")
                                        .trim_end_matches('\n'),
                                    &span,
                                    &mut diagnostics,
                                )
                                .context("adding spans")?;
                            }

                            for children in message.children.into_iter() {
                                for span in children.spans.into_iter() {
                                    Self::maybe_add_diagnostic(
                                        project_root,
                                        &children.level,
                                        &children.message,
                                        &span,
                                        &mut diagnostics,
                                    )
                                    .context("adding child spans")?;
                                }
                            }
                        }
                    }
                    Err(e) => tracing::error!("error deserializing cargo line:\n{line}\n{e}"),
                }
            }
            anyhow::Ok(())
        };

        let stderr_future = async {
            let mut errors = String::new();
            let mut reader = tokio::io::BufReader::new(stderr).lines();
            while let Some(line) = reader.next_line().await? {
                // cargo uses `\r` to keep the progress bar on one line
                // so we resplit on it to make sure we don't miss an update
                for segment in line.split('\r') {
                    let segment = segment.trim();
                    if segment.is_empty() {
                        continue;
                    }
                    if log_cargo != "off" {
                        tracing::info!("[cargo stderr]{segment}");
                    }
                    let trimmed = segment.trim_start();
                    if let Some((message, pct)) = parse_building_line(trimmed) {
                        progress.report_with_message(message, pct).await;
                    } else if let Some(msg) = trimmed.strip_prefix("Blocking") {
                        progress.report_with_message(msg.trim_start(), 0).await;
                    } else if let Some(msg) = trimmed.strip_prefix("error:") {
                        // This will catch things that looks like this
                        // 1) `error: no such command: `fake`
                        // 2) `error: expected `;`, found keyword `let`
                        //
                        // However we are only really interested in the `1)`
                        errors.push_str(msg);
                    }
                }
            }
            anyhow::Ok(errors)
        };

        let (stdout_result, stderr_result) = tokio::join!(stdout_future, stderr_future);

        // If something failed when parsing stdout we consider this an error as
        // diagnostics are parsed from stdout
        stdout_result?;

        // However we don't consider failing to parse stderr as bad
        let logged_errors = match stderr_result {
            Ok(logged_errors) => logged_errors,
            Err(e) => {
                tracing::warn!("error reading cargo stderr: {e}");
                String::new()
            }
        };

        let status = child.wait().await?;
        tracing::debug!("cargo command finished with status {status}");

        // We can't rely on exit code, as cargo exit with the same code regardless if its because
        // of the args / invalid command or because the check fails due to the code being check
        // has errors.
        //
        // So we do hacky thing we consider that the command was likely invalid if there are some
        // error logs but no diagnostics
        if !logged_errors.is_empty() && diagnostics.is_empty() {
            anyhow::bail!("cargo exited with {status}:{logged_errors}");
        }

        tracing::debug!("diags inner: {diagnostics:?}");
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

    pub(crate) async fn find_project_root(params: &InitializeParams) -> Option<PathBuf> {
        let git_root = Self::find_git_root_directory().await?;

        // We only chose the git root as our workspace root if a
        // `Cargo.toml` actually exists in the git root.
        if git_root.join("Cargo.toml").exists() {
            return Some(git_root);
        }

        if let Some(workspace_folders) = &params.workspace_folders {
            for folder in workspace_folders {
                let root_path = PathBuf::from(folder.uri.path().as_str());
                if root_path.join("Cargo.toml").exists() {
                    return Some(root_path);
                }
            }
        }

        #[allow(deprecated)]
        if let Some(root_uri) = &params.root_uri {
            let root_path = PathBuf::from(root_uri.path().as_str());
            if root_path.join("Cargo.toml").exists() {
                return Some(root_path);
            }
        }

        #[allow(deprecated)]
        if let Some(root_path) = &params.root_path {
            let root_path = PathBuf::from(root_path);
            if root_path.join("Cargo.toml").exists() {
                return Some(root_path);
            }
        }

        let cwd = std::env::current_dir().ok()?;
        if cwd.join("Cargo.toml").exists() {
            return Some(cwd);
        }

        None
    }
}

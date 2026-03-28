use std::{
    env,
    path::{Path, PathBuf},
    process::Stdio,
};

use anyhow::Context;
use ls_types::{Diagnostic, DiagnosticSeverity, InitializeParams, Position, Range, Uri};
use serde::{Deserialize, Deserializer};
use tokio::io::{AsyncBufRead, AsyncBufReadExt};
use tokio::process::Command;
use tower_lsp_server::{Bounded, NotCancellable, OngoingProgress};

use crate::{DiagnosticData, PKG_NAME};

/// Like `read_until` but stops at either `\r` or `\n`.
/// Returns the number of bytes read (0 = EOF).
/// The delimiter byte is consumed but not included in `buf`.
async fn read_until_cr_or_lf<R: AsyncBufRead + Unpin>(reader: &mut R, buf: &mut Vec<u8>) -> std::io::Result<usize> {
    let mut total = 0;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(total);
        }
        if let Some(pos) = available.iter().position(|&b| b == b'\r' || b == b'\n') {
            buf.extend_from_slice(&available[..pos]);
            let consumed = pos + 1;
            total += consumed;
            reader.consume(consumed);
            return Ok(total);
        }
        buf.extend_from_slice(available);
        let len = available.len();
        total += len;
        reader.consume(len);
    }
}

#[derive(Debug, Deserialize)]
struct CargoExpansion {
    span: CargoSpan,
}

#[derive(Debug, Deserialize)]
struct CargoSpan {
    #[serde(deserialize_with = "deserialize_url")]
    file_name: Uri,
    line_start: u32,
    line_end: u32,
    column_start: u32,
    column_end: u32,
    suggested_replacement: Option<String>,
    #[serde(default)]
    expansion: Option<Box<CargoExpansion>>,
}

impl CargoSpan {
    /// Returns the innermost span in the macro expansion chain that points to a
    /// project-local (relative-path) file. Falls back to `self` when no such
    /// span exists (e.g. errors purely within a proc-macro).
    fn project_span(&self) -> &Self {
        let is_relative = self
            .file_name
            .authority()
            .map(|a| !a.host().is_empty())
            .unwrap_or(false);
        if is_relative {
            return self;
        }
        if let Some(exp) = &self.expansion {
            return exp.span.project_span();
        }
        self
    }
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
/// ```text
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

    async fn maybe_add_diagnostic(
        project_root: Option<&PathBuf>,
        severity: &str,
        message: &str,
        span: &CargoSpan,
        tx: &flume::Sender<(Uri, Diagnostic)>,
    ) -> anyhow::Result<bool> {
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
                tracing::trace!(?root_dir, ?host, file_name = ?span.file_name, file_name_str = span.file_name.to_string(), "building uri");
                tracing::trace!("replaced path: {}", span.file_name.path().as_str().replacen("/", "", 1));

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
            tracing::trace!(uri = url.to_string(), ?host, "maybe adding diagnostic");
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
            tracing::trace!(uri = url.to_string(), ?diagnostic, "adding diagnostic");
            tx.send_async((url, diagnostic)).await?;
            return Ok(true);
        }

        Ok(false)
    }

    pub(crate) async fn cargo_diagnostics(
        command_args: Vec<String>,
        cargo_env: &[(String, String)],
        project_root: Option<&PathBuf>,
        destination_folder: &Path,
        progress: &OngoingProgress<Bounded, NotCancellable>,
        tx: flume::Sender<(Uri, Diagnostic)>,
    ) -> anyhow::Result<()> {
        tracing::info!(cwd = ?destination_folder, "running cargo {}", command_args.join(" "));
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

        let log_cargo = env::var("BACON_LS_LOG_CARGO").unwrap_or("off".to_string());

        let stdout_future = async {
            let mut at_least_one_diag = false;
            let mut reader = tokio::io::BufReader::new(stdout).lines();
            while let Some(line) = reader.next_line().await? {
                match serde_json::from_str::<CargoLine>(&line) {
                    Ok(message) => {
                        if let Some(message) = message.message
                            && message.message_type == "diagnostic"
                        {
                            let rendered = ansi_regex::ansi_regex()
                                .replace_all(&message.rendered, "")
                                .into_owned();
                            let rendered = rendered.trim_end_matches('\n');
                            for span in &message.spans {
                                at_least_one_diag |= Self::maybe_add_diagnostic(
                                    project_root,
                                    &message.level,
                                    rendered,
                                    span.project_span(),
                                    &tx,
                                )
                                .await
                                .context("adding spans")?;
                            }

                            for children in &message.children {
                                for span in &children.spans {
                                    at_least_one_diag |= Self::maybe_add_diagnostic(
                                        project_root,
                                        &children.level,
                                        &children.message,
                                        span.project_span(),
                                        &tx,
                                    )
                                    .await
                                    .context("adding child spans")?;
                                }
                            }
                        }
                    }
                    Err(e) => tracing::error!("error deserializing cargo line:\n{line}\n{e}"),
                }
            }
            anyhow::Ok(at_least_one_diag)
        };

        let stderr_future = async {
            let mut errors = String::new();
            let mut reader = tokio::io::BufReader::new(stderr);
            let mut buf = Vec::new();
            // Read until \r or \n so cargo progress updates arrive immediately
            // instead of being buffered until the next \n
            loop {
                buf.clear();
                let bytes_read = read_until_cr_or_lf(&mut reader, &mut buf).await?;
                if bytes_read == 0 {
                    break;
                }
                let segment = String::from_utf8_lossy(&buf);
                let segment = segment.trim();
                if segment.is_empty() {
                    continue;
                }
                if log_cargo != "off" {
                    tracing::trace!("[cargo stderr]{segment}");
                }
                let trimmed = segment.trim_start();
                if let Some((message, pct)) = parse_building_line(trimmed) {
                    tracing::trace!(msg = message, pct = pct, "reported Building");
                    progress.report_with_message(message, pct).await;
                } else if let Some(msg) = trimmed.strip_prefix("Blocking") {
                    tracing::trace!(msg = msg, "reported Blocking");
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
            anyhow::Ok(errors)
        };

        let (stdout_result, stderr_result) = tokio::join!(stdout_future, stderr_future);

        // If something failed when parsing stdout we consider this an error as
        // diagnostics are parsed from stdout
        let at_least_one_diag = stdout_result?;

        // However we don't consider failing to parse stderr as bad
        let logged_errors = match stderr_result {
            Ok(logged_errors) => logged_errors,
            Err(e) => {
                tracing::warn!("error reading cargo stderr: {e}");
                String::new()
            }
        };

        let status = child.wait().await?;
        tracing::info!("cargo finished with status {status}");

        // We can't rely on exit code, as cargo exit with the same code regardless if its because
        // of the args / invalid command or because the check fails due to the code being check
        // has errors.
        //
        // So we do hacky thing we consider that the command was likely invalid if there are some
        // error logs but no diagnostics
        if !logged_errors.is_empty() && !at_least_one_diag {
            anyhow::bail!("cargo exited with {status}:{logged_errors}");
        }

        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    // Real cargo JSON line captured from a tokio::select! type error. The
    // primary spans all point into the tokio registry source; only the
    // innermost expansion span resolves back to `src/lib.rs`.
    const TOKIO_SELECT_EXPANSION: &str =
        include_str!("testdata/expansion-needed.json");

    #[test]
    fn test_project_span_follows_macro_expansion_chain() {
        let line: CargoLine = serde_json::from_str(TOKIO_SELECT_EXPANSION).unwrap();
        let message = line.message.unwrap();
        assert_eq!(message.message_type, "diagnostic");

        for span in &message.spans {
            let resolved = span.project_span();
            // The innermost expansion span points to src/lib.rs lines 708-715.
            let host = resolved.file_name.authority().map(|a| a.host().to_string());
            assert_eq!(
                host,
                Some("src".to_string()),
                "expected project-local span, got {:?}",
                resolved.file_name
            );
            assert_eq!(resolved.line_start, 708);
            assert_eq!(resolved.line_end, 715);
        }
    }

    #[test]
    fn test_project_span_returns_self_when_already_project_local() {
        // A span whose file_name is already a relative (project-local) path
        // should be returned as-is without traversing the expansion chain.
        let json = r#"{
            "file_name": "src/main.rs",
            "byte_start": 0, "byte_end": 10,
            "line_start": 1, "line_end": 1,
            "column_start": 1, "column_end": 10,
            "is_primary": true,
            "text": [],
            "label": null,
            "suggested_replacement": null,
            "suggestion_applicability": null,
            "expansion": null
        }"#;
        let span: CargoSpan = serde_json::from_str(json).unwrap();
        let resolved = span.project_span();
        assert!(std::ptr::eq(resolved, &span));
    }

    #[test]
    fn test_project_span_falls_back_to_self_when_no_project_span_in_chain() {
        // When every span in the expansion chain points to an external
        // (absolute-path) file, project_span falls back to self.
        let json = r#"{
            "file_name": "/home/user/.cargo/registry/src/foo/lib.rs",
            "byte_start": 0, "byte_end": 10,
            "line_start": 5, "line_end": 5,
            "column_start": 1, "column_end": 10,
            "is_primary": true,
            "text": [],
            "label": null,
            "suggested_replacement": null,
            "suggestion_applicability": null,
            "expansion": null
        }"#;
        let span: CargoSpan = serde_json::from_str(json).unwrap();
        let resolved = span.project_span();
        assert!(std::ptr::eq(resolved, &span));
    }
}

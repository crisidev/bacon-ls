use std::{
    env,
    path::{Path, PathBuf},
    process::Stdio,
};

use anyhow::Context;
use ls_types::{
    Diagnostic, DiagnosticRelatedInformation, DiagnosticSeverity, InitializeParams, Location, Position, Range, Uri,
};
use serde::{Deserialize, Deserializer};
use tokio::io::{AsyncBufRead, AsyncBufReadExt};
use tokio::process::Command;
use tower_lsp_server::{Bounded, NotCancellable, OngoingProgress};

use crate::{BaconLs, Correction, CorrectionEdit, DiagnosticData, PKG_NAME, path_to_file_uri};

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
    suggestion_applicability: Option<String>,
    #[serde(default)]
    is_primary: bool,
    #[serde(default)]
    expansion: Option<Box<CargoExpansion>>,
}

impl CargoSpan {
    fn is_machine_applicable(&self) -> bool {
        self.suggestion_applicability.as_deref() == Some("MachineApplicable")
    }

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
    str::parse::<Uri>(&path_to_file_uri(url_str)).map_err(serde::de::Error::custom)
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
            "error" => DiagnosticSeverity::ERROR,
            "warning" | "failure-note" => DiagnosticSeverity::WARNING,
            "info" | "information" | "note" => DiagnosticSeverity::INFORMATION,
            "hint" | "help" => DiagnosticSeverity::HINT,
            other => {
                tracing::warn!("unknown cargo severity level {other:?}, defaulting to INFORMATION");
                DiagnosticSeverity::INFORMATION
            }
        }
    }

    fn span_to_uri(project_root: Option<&PathBuf>, span: &CargoSpan) -> anyhow::Result<Option<Uri>> {
        let Some(host) = span.file_name.authority().map(|auth| auth.host()) else {
            return Ok(None);
        };
        let root_dir = match project_root {
            Some(r) => r.clone(),
            None => env::current_dir().context("getting current dir")?,
        };
        tracing::trace!(?root_dir, ?host, file_name = ?span.file_name, "building uri");
        // If host is empty, the span.file_name is an absolute path.
        let path = if host.is_empty() {
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
        // paths we get passed from the LSP server. A canonicalize failure here means this
        // single span can't be resolved (e.g. file deleted between cargo emitting and us
        // reading): skip it rather than aborting the whole diagnostics run.
        let canonical = match path.canonicalize() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "skipping diagnostic span: cannot canonicalize path"
                );
                return Ok(None);
            }
        };
        let file_name = canonical
            .into_os_string()
            .into_string()
            .map_err(|_orig| std::io::Error::other("cannot convert file name to string"))?;
        Ok(Some(
            str::parse::<Uri>(&path_to_file_uri(&file_name)).context("parsing filename")?,
        ))
    }

    async fn maybe_add_diagnostic(
        project_root: Option<&PathBuf>,
        message: &CargoMessage,
        use_related_information: bool,
        tx: &flume::Sender<(Uri, Diagnostic)>,
    ) -> anyhow::Result<bool> {
        let rendered = ansi_regex::ansi_regex().replace_all(&message.rendered, "").into_owned();
        let rendered = rendered.trim_end_matches('\n');

        // When the client supports related information (e.g. VS Code), attach
        // children with primary spans to the parent diagnostic as clickable
        // links. Otherwise emit them as separate diagnostics so editors like
        // neovim can display them directly.
        let related_information = if use_related_information {
            let info: Vec<DiagnosticRelatedInformation> = message
                .children
                .iter()
                .flat_map(|child| {
                    child
                        .spans
                        .iter()
                        .filter(|s| s.is_primary)
                        .filter_map(|s| {
                            Self::span_to_uri(project_root, s.project_span())
                                .ok()
                                .flatten()
                                .map(|uri| DiagnosticRelatedInformation {
                                    location: Location {
                                        uri,
                                        range: Range::new(
                                            Position::new(
                                                s.line_start.saturating_sub(1),
                                                s.column_start.saturating_sub(1),
                                            ),
                                            Position::new(s.line_end.saturating_sub(1), s.column_end.saturating_sub(1)),
                                        ),
                                    },
                                    message: child.message.clone(),
                                })
                        })
                        .collect::<Vec<_>>()
                })
                .collect();
            if info.is_empty() { None } else { Some(info) }
        } else {
            None
        };

        // A cargo message can have several primary spans pointing to different
        // project locations (e.g. conflicting trait impls, lifetime conflicts
        // across multiple binding sites). We emit a diagnostic for every span
        // that resolves to a project-local URI; spans that don't (pure-macro
        // or registry paths) are skipped via the `?` below.
        let mut at_least_one = false;
        for span in message.spans.iter().filter(|s| s.is_primary) {
            let resolved = span.project_span();
            let Some(url) = Self::span_to_uri(project_root, resolved)? else {
                continue;
            };
            tracing::trace!(uri = url.to_string(), "adding diagnostic");
            let range = Range::new(
                Position::new(
                    resolved.line_start.saturating_sub(1),
                    resolved.column_start.saturating_sub(1),
                ),
                Position::new(
                    resolved.line_end.saturating_sub(1),
                    resolved.column_end.saturating_sub(1),
                ),
            );
            let corrections: Vec<Correction> = resolved
                .is_machine_applicable()
                .then_some(resolved.suggested_replacement.as_deref())
                .flatten()
                .map(|text| Correction::from_single(range, text))
                .into_iter()
                .collect();
            let data = if corrections.is_empty() {
                None
            } else {
                Some(serde_json::json!(DiagnosticData { corrections }))
            };
            let diagnostic = Diagnostic {
                range,
                severity: Some(Self::parse_severity(&message.level)),
                source: Some(PKG_NAME.to_string()),
                message: rendered.to_string(),
                related_information: related_information.clone(),
                data,
                ..Diagnostic::default()
            };
            tracing::trace!(uri = url.to_string(), ?diagnostic, "sending diagnostic");
            tx.send_async((url, diagnostic)).await?;
            at_least_one = true;
        }

        // When the client does not support related information, emit children
        // with primary spans as separate diagnostics.
        if !use_related_information {
            for child in &message.children {
                let Some(first_primary) = child.spans.iter().find(|s| s.is_primary) else {
                    continue;
                };
                let resolved = first_primary.project_span();
                let Some(url) = Self::span_to_uri(project_root, resolved)? else {
                    continue;
                };
                let range = Range::new(
                    Position::new(
                        resolved.line_start.saturating_sub(1),
                        resolved.column_start.saturating_sub(1),
                    ),
                    Position::new(
                        resolved.line_end.saturating_sub(1),
                        resolved.column_end.saturating_sub(1),
                    ),
                );

                // Group all MachineApplicable primary spans within this child
                // into a single Correction so editors can apply them atomically
                // (e.g. removing "Compact" from "{Compact, FmtSpan}" requires
                // three separate byte-range deletions in one edit).
                let edits: Vec<CorrectionEdit> = child
                    .spans
                    .iter()
                    .filter(|s| s.is_primary && s.is_machine_applicable())
                    .filter_map(|s| {
                        let new_text = s.suggested_replacement.as_deref()?;
                        let r = s.project_span();
                        Some(CorrectionEdit {
                            range: Range::new(
                                Position::new(r.line_start.saturating_sub(1), r.column_start.saturating_sub(1)),
                                Position::new(r.line_end.saturating_sub(1), r.column_end.saturating_sub(1)),
                            ),
                            new_text: new_text.to_string(),
                        })
                    })
                    .collect();
                let corrections = if edits.is_empty() {
                    vec![]
                } else {
                    vec![Correction::from_multi(edits)]
                };
                let data = if corrections.is_empty() {
                    None
                } else {
                    Some(serde_json::json!(DiagnosticData { corrections }))
                };
                let diagnostic = Diagnostic {
                    range,
                    severity: Some(Self::parse_severity(&child.level)),
                    source: Some(PKG_NAME.to_string()),
                    message: child.message.clone(),
                    data,
                    ..Diagnostic::default()
                };
                tracing::trace!(uri = url.to_string(), ?diagnostic, "sending child diagnostic");
                tx.send_async((url, diagnostic)).await?;
                at_least_one = true;
            }
        }

        Ok(at_least_one)
    }

    pub(crate) async fn cargo_diagnostics(
        command_args: Vec<String>,
        cargo_env: &[(String, String)],
        project_root: Option<&PathBuf>,
        destination_folder: &Path,
        use_related_information: bool,
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
            // Close stdin explicitly: the LSP server's stdin is the jsonrpc
            // pipe, and we must not let cargo (or any tool it invokes) inherit
            // and read from it.
            .stdin(Stdio::null())
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
                            at_least_one_diag |=
                                Self::maybe_add_diagnostic(project_root, &message, use_related_information, &tx)
                                    .await
                                    .context("adding diagnostic")?;
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

    pub(crate) async fn find_project_root(params: &InitializeParams) -> Option<PathBuf> {
        // Build an ordered list of candidate paths to probe:
        // client-authoritative workspace folders first, then the deprecated
        // root_uri/root_path fields, then the server's own CWD as a last
        // resort. This avoids ever picking the LSP server's CWD over an
        // explicitly-specified workspace folder.
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(workspace_folders) = &params.workspace_folders {
            for folder in workspace_folders {
                candidates.push(PathBuf::from(folder.uri.path().as_str()));
            }
        }
        #[allow(deprecated)]
        if let Some(root_uri) = &params.root_uri {
            candidates.push(PathBuf::from(root_uri.path().as_str()));
        }
        #[allow(deprecated)]
        if let Some(root_path) = &params.root_path {
            candidates.push(PathBuf::from(root_path));
        }
        if let Ok(cwd) = std::env::current_dir() {
            candidates.push(cwd);
        }

        for candidate in &candidates {
            // Prefer the git root containing this candidate when it has a
            // Cargo.toml — this lets a nested crate resolve to the workspace
            // root, which is where cargo needs to run.
            if let Some(git_root) = BaconLs::find_git_root_directory(candidate).await
                && git_root.join("Cargo.toml").exists()
            {
                return Some(git_root);
            }
            if candidate.join("Cargo.toml").exists() {
                return Some(candidate.clone());
            }
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
    const TOKIO_SELECT_EXPANSION: &str = include_str!("testdata/expansion-needed.json");

    // Real cargo JSON line for an "unused variable" warning. Has two children:
    // one with no spans (dropped into rendered message) and one help child with
    // a primary span carrying a suggested_replacement (becomes relatedInformation).
    const UNUSED_VARIABLE: &str = include_str!("testdata/unused-variable.json");

    // Unused import of a whole `use` item — child span covers the full line
    // including newline, suggested_replacement="" → "Remove" correction label.
    const UNUSED_IMPORT_LINE: &str = include_str!("testdata/unused-import-line.json");

    // Unused import inside a grouped `use` — the help child has three primary
    // spans (identifier + surrounding punctuation), all suggested_replacement=""
    // → three "Remove" corrections.
    const UNUSED_IMPORT_GROUPED: &str = include_str!("testdata/unused-import-compact.json");

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
    fn test_children_structure_for_separate_diagnostics() {
        let line: CargoLine = serde_json::from_str(UNUSED_VARIABLE).unwrap();
        let message = line.message.unwrap();
        assert_eq!(message.message_type, "diagnostic");
        assert_eq!(message.level, "warning");

        // One primary span pointing directly to project source — no expansion needed.
        let primary_spans: Vec<_> = message.spans.iter().filter(|s| s.is_primary).collect();
        assert_eq!(primary_spans.len(), 1);
        let span = primary_spans[0];
        assert_eq!(span.project_span().line_start, 719);
        assert!(
            span.file_name
                .authority()
                .map(|a| !a.host().is_empty())
                .unwrap_or(false),
            "primary span should already be project-local"
        );

        // Two children: first has no spans (skipped — text in rendered message),
        // second has a primary span with a suggested_replacement and becomes
        // its own diagnostic.
        assert_eq!(message.children.len(), 2);
        assert!(message.children[0].spans.is_empty());
        assert_eq!(message.children[0].level, "note");
        let help_child = &message.children[1];
        assert_eq!(help_child.level, "help");
        assert_eq!(
            help_child.message,
            "if this is intentional, prefix it with an underscore"
        );
        let help_spans: Vec<_> = help_child.spans.iter().filter(|s| s.is_primary).collect();
        assert_eq!(help_spans.len(), 1);
        assert_eq!(help_spans[0].suggested_replacement.as_deref(), Some("_lol"));

        // The child's MachineApplicable span becomes a correction on the
        // child's own diagnostic (not the parent's).
        let help_child_spans: Vec<_> = help_child
            .spans
            .iter()
            .filter(|s| s.is_primary && s.is_machine_applicable())
            .collect();
        assert_eq!(help_child_spans.len(), 1);
        assert_eq!(help_child_spans[0].suggested_replacement.as_deref(), Some("_lol"));
    }

    #[test]
    fn test_unused_import_whole_line_produces_remove_correction() {
        let line: CargoLine = serde_json::from_str(UNUSED_IMPORT_LINE).unwrap();
        let message = line.message.unwrap();
        assert_eq!(message.level, "warning");

        let primary_spans: Vec<_> = message.spans.iter().filter(|s| s.is_primary).collect();
        assert_eq!(primary_spans.len(), 1);
        // Primary span has no replacement; it's the child that carries the fix.
        assert!(primary_spans[0].suggested_replacement.is_none());

        // One child with one primary span carrying an empty replacement → one
        // Correction with one edit, labelled "Remove".
        assert_eq!(message.children.len(), 2);
        let help_child = &message.children[1];
        let primary_spans: Vec<_> = help_child.spans.iter().filter(|s| s.is_primary).collect();
        assert_eq!(primary_spans.len(), 1);
        assert_eq!(primary_spans[0].suggested_replacement.as_deref(), Some(""));
        let resolved = primary_spans[0].project_span();
        let range = Range::new(
            Position::new(
                resolved.line_start.saturating_sub(1),
                resolved.column_start.saturating_sub(1),
            ),
            Position::new(
                resolved.line_end.saturating_sub(1),
                resolved.column_end.saturating_sub(1),
            ),
        );
        let correction = Correction::from_single(range, "");
        assert_eq!(correction.label, "Remove");
        assert_eq!(correction.edits.len(), 1);
    }

    #[test]
    fn test_unused_import_grouped_produces_three_remove_corrections() {
        let line: CargoLine = serde_json::from_str(UNUSED_IMPORT_GROUPED).unwrap();
        let message = line.message.unwrap();
        assert_eq!(message.level, "warning");

        // One child with three primary spans (identifier + comma + surrounding
        // space), all with empty suggested_replacement. They must be grouped
        // into a single Correction with three CorrectionEdits so the editor
        // applies all three byte-range deletions atomically.
        assert_eq!(message.children.len(), 2);
        let help_child = &message.children[1];
        let primary_spans: Vec<_> = help_child
            .spans
            .iter()
            .filter(|s| s.is_primary && s.is_machine_applicable())
            .collect();
        assert_eq!(primary_spans.len(), 3, "three spans for grouped import removal");
        let edits: Vec<CorrectionEdit> = primary_spans
            .iter()
            .filter_map(|s| {
                let new_text = s.suggested_replacement.as_deref()?;
                let resolved = s.project_span();
                Some(CorrectionEdit {
                    range: Range::new(
                        Position::new(
                            resolved.line_start.saturating_sub(1),
                            resolved.column_start.saturating_sub(1),
                        ),
                        Position::new(
                            resolved.line_end.saturating_sub(1),
                            resolved.column_end.saturating_sub(1),
                        ),
                    ),
                    new_text: new_text.to_string(),
                })
            })
            .collect();
        assert_eq!(edits.len(), 3);
        let correction = Correction::from_multi(edits);
        assert_eq!(correction.label, "Remove");
        assert_eq!(correction.edits.len(), 3);
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

    #[test]
    fn test_parse_building_line_basic() {
        // The returned message is the portion after "] " (i.e. fraction + crate list),
        // not the full original line — that's the piece suitable for a progress report.
        let line = "Building [====       ] 7/10: foo, bar";
        let (msg, pct) = parse_building_line(line).unwrap();
        assert_eq!(msg, "7/10: foo, bar");
        assert_eq!(pct, 70);
    }

    #[test]
    fn test_parse_building_line_caps_percentage_at_99() {
        let line = "Building [========] 10/10: last";
        let (_, pct) = parse_building_line(line).unwrap();
        assert_eq!(
            pct, 99,
            "100% should cap at 99 to keep the bar indeterminate until Finished"
        );
    }

    #[test]
    fn test_parse_building_line_rejects_non_building_prefix() {
        assert!(parse_building_line("Finished `dev` profile").is_none());
        assert!(parse_building_line("Compiling foo v0.1.0").is_none());
    }

    #[test]
    fn test_parse_building_line_zero_total_returns_none() {
        assert!(parse_building_line("Building [] 0/0: nothing").is_none());
    }

    #[test]
    fn test_parse_building_line_malformed_returns_none() {
        assert!(parse_building_line("Building [====] no-fraction-here").is_none());
        assert!(parse_building_line("Building [====] abc/def: foo").is_none());
    }

    #[test]
    fn test_parse_severity_known_levels() {
        assert_eq!(Cargo::parse_severity("error"), DiagnosticSeverity::ERROR);
        assert_eq!(Cargo::parse_severity("warning"), DiagnosticSeverity::WARNING);
        assert_eq!(Cargo::parse_severity("failure-note"), DiagnosticSeverity::WARNING);
        assert_eq!(Cargo::parse_severity("note"), DiagnosticSeverity::INFORMATION);
        assert_eq!(Cargo::parse_severity("info"), DiagnosticSeverity::INFORMATION);
        assert_eq!(Cargo::parse_severity("help"), DiagnosticSeverity::HINT);
        assert_eq!(Cargo::parse_severity("hint"), DiagnosticSeverity::HINT);
    }

    #[test]
    fn test_parse_severity_unknown_level_defaults_to_information() {
        assert_eq!(
            Cargo::parse_severity("something-brand-new"),
            DiagnosticSeverity::INFORMATION
        );
    }

    #[tokio::test]
    async fn test_read_until_lf_consumes_terminator() {
        let mut reader = tokio::io::BufReader::new(&b"hello\nworld"[..]);
        let mut buf = Vec::new();
        let n = read_until_cr_or_lf(&mut reader, &mut buf).await.unwrap();
        // 6 bytes: "hello" + the LF (LF is consumed but not pushed into buf)
        assert_eq!(n, 6);
        assert_eq!(buf, b"hello");
    }

    #[tokio::test]
    async fn test_read_until_cr_consumes_terminator() {
        // Cargo emits progress lines terminated only by `\r` — that's the case
        // this helper exists for.
        let mut reader = tokio::io::BufReader::new(&b"progress\rnext"[..]);
        let mut buf = Vec::new();
        let n = read_until_cr_or_lf(&mut reader, &mut buf).await.unwrap();
        assert_eq!(n, 9);
        assert_eq!(buf, b"progress");
    }

    #[tokio::test]
    async fn test_read_until_cr_or_lf_returns_zero_at_eof() {
        let mut reader = tokio::io::BufReader::new(&b""[..]);
        let mut buf = Vec::new();
        let n = read_until_cr_or_lf(&mut reader, &mut buf).await.unwrap();
        assert_eq!(n, 0);
        assert!(buf.is_empty());
    }

    #[tokio::test]
    async fn test_read_until_cr_or_lf_no_terminator_reads_to_eof() {
        let mut reader = tokio::io::BufReader::new(&b"trailing"[..]);
        let mut buf = Vec::new();
        let n = read_until_cr_or_lf(&mut reader, &mut buf).await.unwrap();
        assert_eq!(n, 8);
        assert_eq!(buf, b"trailing");
    }

    #[tokio::test]
    async fn test_read_until_cr_or_lf_spans_multiple_internal_buffers() {
        // BufReader with capacity 4 forces multiple `fill_buf` rounds before
        // we hit the terminator at byte 9.
        let mut reader = tokio::io::BufReader::with_capacity(4, &b"abcdefghi\nrest"[..]);
        let mut buf = Vec::new();
        let n = read_until_cr_or_lf(&mut reader, &mut buf).await.unwrap();
        assert_eq!(n, 10);
        assert_eq!(buf, b"abcdefghi");
    }

    fn make_relative_span(file_name_str: &str, is_primary: bool) -> CargoSpan {
        // Construct via JSON to mirror real cargo output (and avoid duplicating
        // the deserialize_url logic).
        let json = format!(
            r#"{{
                "file_name": "{file_name_str}",
                "byte_start": 0, "byte_end": 0,
                "line_start": 1, "line_end": 1,
                "column_start": 1, "column_end": 1,
                "is_primary": {is_primary},
                "text": [],
                "label": null,
                "suggested_replacement": null,
                "suggestion_applicability": null,
                "expansion": null
            }}"#
        );
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn test_span_to_uri_returns_none_when_no_authority() {
        // file_name without `file://` scheme → no authority → not a project
        // span we can resolve. The deserialize_url helper prepends `file://`,
        // so we go through deserialization explicitly.
        let span = make_relative_span("/absolute/path/that/will/not/exist", true);
        // Authority is present (host=""); span_to_uri proceeds, hits
        // canonicalize on the absolute path, fails, returns Ok(None).
        let result = Cargo::span_to_uri(None, &span).unwrap();
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn test_span_to_uri_returns_none_when_canonicalize_fails() {
        // Relative path under a project root that does not exist on disk —
        // canonicalize fails and we skip rather than aborting.
        let tmp = tempfile::TempDir::new().unwrap();
        let span = make_relative_span("does/not/exist.rs", true);
        let result = Cargo::span_to_uri(Some(&tmp.path().to_path_buf()), &span).unwrap();
        assert_eq!(result, None, "missing file should be skipped, not error");
    }

    #[tokio::test]
    async fn test_span_to_uri_resolves_existing_relative_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src_dir = tmp.path().join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        let lib_rs = src_dir.join("lib.rs");
        std::fs::write(&lib_rs, "// content").unwrap();

        let span = make_relative_span("src/lib.rs", true);
        let uri = Cargo::span_to_uri(Some(&tmp.path().to_path_buf()), &span)
            .unwrap()
            .expect("existing path should resolve");

        let canonical = lib_rs.canonicalize().unwrap();
        let expected = format!("file://{}", canonical.display());
        assert_eq!(uri.to_string(), expected);
    }

    #[tokio::test]
    async fn test_maybe_add_diagnostic_emits_per_primary_span() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src_dir = tmp.path().join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(src_dir.join("lib.rs"), "// content").unwrap();
        let project_root = tmp.path().to_path_buf();

        let line: CargoLine = serde_json::from_str(UNUSED_VARIABLE).unwrap();
        let message = line.message.unwrap();

        let (tx, rx) = flume::unbounded();
        // use_related_information=true: the help-child span attaches as
        // related info on the parent diagnostic, not its own diagnostic.
        let any = Cargo::maybe_add_diagnostic(Some(&project_root), &message, true, &tx)
            .await
            .unwrap();
        drop(tx);

        assert!(any, "should emit at least one diagnostic for the unused variable");
        let received: Vec<_> = rx.drain().collect();
        assert_eq!(received.len(), 1, "one primary span → one diagnostic");
        let (_uri, diag) = &received[0];
        assert_eq!(diag.severity, Some(DiagnosticSeverity::WARNING));
        assert_eq!(diag.source, Some(PKG_NAME.to_string()));
        assert!(
            diag.related_information
                .as_ref()
                .is_some_and(|r| !r.is_empty()),
            "help child should attach as related information when supported"
        );
    }

    #[tokio::test]
    async fn test_maybe_add_diagnostic_separate_children_when_unsupported() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src_dir = tmp.path().join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(src_dir.join("lib.rs"), "// content").unwrap();
        let project_root = tmp.path().to_path_buf();

        let line: CargoLine = serde_json::from_str(UNUSED_VARIABLE).unwrap();
        let message = line.message.unwrap();

        let (tx, rx) = flume::unbounded();
        // use_related_information=false: the help child is emitted as its own
        // diagnostic.
        let any = Cargo::maybe_add_diagnostic(Some(&project_root), &message, false, &tx)
            .await
            .unwrap();
        drop(tx);

        assert!(any);
        let received: Vec<_> = rx.drain().collect();
        // 1 parent (warning) + 1 help child with primary span = 2 diagnostics.
        assert_eq!(received.len(), 2, "parent + help child as separate diagnostics");
        let levels: Vec<_> = received.iter().map(|(_, d)| d.severity).collect();
        assert!(levels.contains(&Some(DiagnosticSeverity::WARNING)));
        assert!(levels.contains(&Some(DiagnosticSeverity::HINT)));

        // The help child carries a MachineApplicable replacement → child
        // diagnostic should expose the correction in `data`.
        let help_diag = received
            .iter()
            .find(|(_, d)| d.severity == Some(DiagnosticSeverity::HINT))
            .unwrap();
        assert!(help_diag.1.data.is_some(), "help child should carry quick-fix data");
    }

    #[tokio::test]
    async fn test_find_project_root_picks_workspace_folder_with_cargo_toml() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[package]\nname = \"x\"").unwrap();

        let folder_uri = format!("file://{}", tmp.path().display());
        let params: InitializeParams = serde_json::from_value(serde_json::json!({
            "processId": null,
            "rootUri": null,
            "capabilities": {},
            "workspaceFolders": [{
                "uri": folder_uri,
                "name": "test"
            }]
        }))
        .unwrap();

        let root = Cargo::find_project_root(&params).await;
        let canonical = tmp.path().canonicalize().unwrap();
        assert_eq!(
            root.map(|p| p.canonicalize().unwrap()),
            Some(canonical),
            "workspace folder containing Cargo.toml should win"
        );
    }

    #[tokio::test]
    async fn test_find_project_root_returns_none_when_no_cargo_toml_anywhere() {
        // Empty tempdir: no Cargo.toml in any candidate.
        let tmp = tempfile::TempDir::new().unwrap();
        let folder_uri = format!("file://{}", tmp.path().display());
        let params: InitializeParams = serde_json::from_value(serde_json::json!({
            "processId": null,
            "rootUri": folder_uri,
            "capabilities": {},
        }))
        .unwrap();
        let root = Cargo::find_project_root(&params).await;
        // The CWD fallback may still find a Cargo.toml when tests run from the
        // repo, so we only assert: if Some, it's not the empty tempdir.
        if let Some(p) = root {
            assert_ne!(
                p.canonicalize().ok(),
                Some(tmp.path().canonicalize().unwrap()),
                "tempdir without Cargo.toml must not be picked"
            );
        }
    }
}

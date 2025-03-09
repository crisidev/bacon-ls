//! Bacon Language Server
use std::borrow::Cow;
use std::collections::HashSet;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use argh::FromArgs;
use bacon::Bacon;
use notify_debouncer_full::{DebounceEventResult, new_debouncer};
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tower_lsp::{
    Client, LspService, Server,
    lsp_types::{Diagnostic, Url, WorkspaceFolder},
};
use tracing_subscriber::fmt::format::FmtSpan;

mod bacon;
mod lsp;

const PKG_NAME: &str = env!("CARGO_PKG_NAME");
pub const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");
const LOCATIONS_FILE: &str = ".bacon-locations";
const BACON_BACKGROUND_COMMAND_ARGS: &str = "--headless -j bacon-ls";

/// bacon-ls - https://github.com/crisidev/bacon-ls
#[derive(Debug, FromArgs)]
pub struct Args {
    /// display version information
    #[argh(switch, short = 'v')]
    pub version: bool,
}

#[derive(Debug, Clone, Copy)]
enum Backend {
    Bacon,
    Native,
}

#[derive(Debug)]
struct State {
    workspace_folders: Option<Vec<WorkspaceFolder>>,
    locations_file: String,
    update_on_save: bool,
    update_on_save_wait_millis: Duration,
    validate_bacon_preferences: bool,
    run_bacon_in_background: bool,
    run_bacon_in_background_command_args: String,
    create_bacon_preferences_file: bool,
    bacon_command_handle: Option<JoinHandle<()>>,
    syncronize_all_open_files_wait_millis: Duration,
    diagnostics_data_supported: bool,
    open_files: HashSet<Url>,
    cancel_token: CancellationToken,
    sync_files_handle: Option<JoinHandle<()>>,
    backend: Backend,
}

impl Default for State {
    fn default() -> Self {
        Self {
            workspace_folders: None,
            locations_file: LOCATIONS_FILE.to_string(),
            update_on_save: true,
            update_on_save_wait_millis: Duration::from_millis(1000),
            validate_bacon_preferences: true,
            run_bacon_in_background: true,
            run_bacon_in_background_command_args: BACON_BACKGROUND_COMMAND_ARGS.to_string(),
            create_bacon_preferences_file: true,
            bacon_command_handle: None,
            syncronize_all_open_files_wait_millis: Duration::from_millis(2000),
            diagnostics_data_supported: false,
            open_files: HashSet::new(),
            cancel_token: CancellationToken::new(),
            sync_files_handle: None,
            backend: Backend::Native,
        }
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct DiagnosticData<'c> {
    corrections: Vec<Cow<'c, str>>,
}

#[derive(Debug, Default)]
pub struct BaconLs {
    client: Option<Arc<Client>>,
    state: Arc<RwLock<State>>,
}

impl BaconLs {
    fn new(client: Client) -> Self {
        Self {
            client: Some(Arc::new(client)),
            state: Arc::new(RwLock::new(State::default())),
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

    async fn find_git_root_directory(path: &Path) -> Option<PathBuf> {
        let output = tokio::process::Command::new("git")
            .arg("-C")
            .arg(path)
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

    fn deduplicate_diagnostics(
        path: Url,
        uri: Option<&Url>,
        diagnostic: Diagnostic,
        diagnostics: &mut Vec<(Url, Diagnostic)>,
    ) {
        if Some(&path) == uri
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

    async fn diagnostics_vec(
        uri: Option<&Url>,
        locations_file_name: &str,
        workspace_folders: Option<&[WorkspaceFolder]>,
        backend: Backend,
    ) -> Vec<Diagnostic> {
        match backend {
            Backend::Bacon => Bacon::diagnostics(uri, locations_file_name, workspace_folders)
                .await
                .into_iter()
                .map(|(_, y)| y)
                .collect::<Vec<Diagnostic>>(),
            Backend::Native => vec![],
        }
    }

    async fn publish_diagnostics(
        client: Option<&Arc<Client>>,
        uri: &Url,
        locations_file_name: &str,
        workspace_folders: Option<&[WorkspaceFolder]>,
        backend: Backend,
    ) {
        if let Some(client) = client {
            client
                .publish_diagnostics(
                    uri.clone(),
                    Self::diagnostics_vec(Some(uri), locations_file_name, workspace_folders, backend).await,
                    None,
                )
                .await;
        }
    }

    async fn syncronize_diagnostics_for_all_open_files(state: Arc<RwLock<State>>, client: Option<Arc<Client>>) {
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

        watcher
            .watch(PathBuf::from(&locations_file), notify::RecursiveMode::Recursive)
            .expect("couldn't watch diagnostics file");

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
            let backend = loop_state.backend;
            drop(loop_state);
            tracing::debug!(
                "running periodic diagnostic publish for open files `{}`",
                open_files.iter().map(|f| f.to_string()).collect::<Vec<_>>().join(",")
            );
            for uri in open_files.iter() {
                Self::publish_diagnostics(
                    client.as_ref(),
                    uri,
                    &locations_file,
                    workspace_folders.as_deref(),
                    backend,
                )
                .await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_can_configure_tracing() {
        BaconLs::configure_tracing(Some("info".to_string()));
    }
}

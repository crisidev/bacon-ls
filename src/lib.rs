//! Bacon Language Server
use std::borrow::Cow;
use std::collections::HashSet;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use argh::FromArgs;
use bacon::Bacon;
use native::Cargo;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tower_lsp::{
    Client, LspService, Server,
    lsp_types::{Url, WorkspaceFolder},
};
use tracing_subscriber::fmt::format::FmtSpan;

mod bacon;
mod lsp;
mod native;

const PKG_NAME: &str = env!("CARGO_PKG_NAME");
pub const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");
const LOCATIONS_FILE: &str = ".bacon-locations";
const BACON_BACKGROUND_COMMAND_ARGS: &str = "--headless -j bacon-ls";
const CARGO_COMMAND_ARGS: &str =
    "clippy --tests --all-features --all-targets --message-format json-diagnostic-rendered-ansi";

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
    Cargo,
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
    diagnostics_version: i32,
    cargo_command_args: String,
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
            backend: Backend::Cargo,
            diagnostics_version: 0,
            cargo_command_args: CARGO_COMMAND_ARGS.to_string(),
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
                .with_target(true)
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

    async fn publish_diagnostics(&self, uri: &Url) {
        let mut guard = self.state.write().await;
        let locations_file_name = guard.locations_file.clone();
        let workspace_folders = guard.workspace_folders.clone();
        let open_files = guard.open_files.clone();
        let backend = guard.backend;
        let command_args = guard.cargo_command_args.clone();
        guard.diagnostics_version += 1;
        let version = guard.diagnostics_version;
        drop(guard);
        match backend {
            Backend::Bacon => {
                Bacon::publish_diagnostics(
                    self.client.as_ref(),
                    uri,
                    &locations_file_name,
                    workspace_folders.as_deref(),
                )
                .await;
            }
            Backend::Cargo => {
                if let Some(client) = self.client.as_ref() {
                    let diagnostics = Cargo::cargo_diagnostics(&command_args).await.unwrap();
                    if !diagnostics.contains_key(uri) {
                        tracing::info!("cleaned up cargo diagnostics for {uri}");
                        client.publish_diagnostics(uri.clone(), vec![], Some(version)).await;
                    }
                    for (uri, diagnostics) in diagnostics.into_iter() {
                        if diagnostics.is_empty() {
                            tracing::info!("cleaned up cargo diagnostics for {uri}");
                            client.publish_diagnostics(uri, vec![], Some(version)).await;
                        } else if open_files.contains(&uri) {
                            tracing::info!("sent {} cargo diagnostics for {uri}", diagnostics.len());
                            client.publish_diagnostics(uri, diagnostics, Some(version)).await;
                        }
                    }
                }
            }
        }
    }

    async fn syncronize_diagnostics(state: Arc<RwLock<State>>, client: Option<Arc<Client>>) {
        let backend = state.read().await.backend;
        if let Backend::Bacon = backend {
            Bacon::syncronize_diagnostics(state, client).await;
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

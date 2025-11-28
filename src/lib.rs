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
use rand::Rng;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tower_lsp_server::lsp_types::ProgressToken;
use tower_lsp_server::{
    Client, LspService, Server,
    lsp_types::{Uri, WorkspaceFolder},
};
use tracing_subscriber::fmt::format::FmtSpan;

mod bacon;
mod lsp;
mod native;

const PKG_NAME: &str = env!("CARGO_PKG_NAME");
pub const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");
const LOCATIONS_FILE: &str = ".bacon-locations";
const BACON_BACKGROUND_COMMAND: &str = "bacon";
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
    project_root: Option<PathBuf>,
    workspace_folders: Option<Vec<WorkspaceFolder>>,
    locations_file: String,
    update_on_save: bool,
    update_on_save_wait_millis: Duration,
    update_on_change: bool,
    update_on_change_cooldown_millis: Duration,
    validate_bacon_preferences: bool,
    run_bacon_in_background: bool,
    run_bacon_in_background_command: String,
    run_bacon_in_background_command_args: String,
    create_bacon_preferences_file: bool,
    bacon_command_handle: Option<JoinHandle<()>>,
    syncronize_all_open_files_wait_millis: Duration,
    diagnostics_data_supported: bool,
    open_files: HashSet<Uri>,
    cancel_token: CancellationToken,
    sync_files_handle: Option<JoinHandle<()>>,
    backend: Backend,
    diagnostics_version: i32,
    cargo_command_args: String,
    cargo_env: Vec<String>,
    build_folder: PathBuf,
    last_change: Instant,
}

impl Default for State {
    fn default() -> Self {
        Self {
            project_root: None,
            workspace_folders: None,
            locations_file: LOCATIONS_FILE.to_string(),
            update_on_save: true,
            update_on_save_wait_millis: Duration::from_millis(1000),
            update_on_change: false,
            update_on_change_cooldown_millis: Duration::from_millis(5000),
            validate_bacon_preferences: true,
            run_bacon_in_background: true,
            run_bacon_in_background_command: BACON_BACKGROUND_COMMAND.to_string(),
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
            cargo_env: vec![],
            build_folder: tempfile::tempdir().unwrap().path().into(),
            last_change: Instant::now(),
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
                .with_target(true)
                .with_file(true)
                .with_line_number(true)
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

    async fn publish_diagnostics(&self, uri: &Uri) {
        let mut guard = self.state.write().await;
        let locations_file_name = guard.locations_file.clone();
        let workspace_folders = guard.workspace_folders.clone();
        let open_files = guard.open_files.clone();
        let backend = guard.backend;
        let command_args = guard.cargo_command_args.clone();
        let cargo_env = guard.cargo_env.clone();
        let project_root = guard.project_root.clone();
        let build_folder = guard.build_folder.clone();
        guard.diagnostics_version += 1;
        let version = guard.diagnostics_version;
        drop(guard);

        tracing::info!(uri = uri.to_string(), "publish diagnostics");
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
                    let token = ProgressToken::Number(rand::rng().random::<i32>());
                    let first_arg = command_args.split_whitespace().next().unwrap_or("check");
                    let progress = client
                        .progress(token, "running:")
                        .with_message(format!("cargo {first_arg}"))
                        .with_percentage(0)
                        .begin()
                        .await;
                    let diagnostics =
                        Cargo::cargo_diagnostics(&command_args, &cargo_env, project_root.as_ref(), &build_folder)
                            .await
                            .inspect_err(|err| tracing::error!(?err, "error building diagnostics"))
                            .unwrap_or_default();
                    progress.report(90).await;
                    if !diagnostics.contains_key(uri) {
                        tracing::info!(
                            uri = uri.to_string(),
                            "cleaned up cargo diagnostics. does not contain key."
                        );
                        client.publish_diagnostics(uri.clone(), vec![], Some(version)).await;
                    }
                    for (uri, diagnostics) in diagnostics.into_iter() {
                        if diagnostics.is_empty() {
                            tracing::info!(uri = uri.to_string(), "cleaned up cargo diagnostics. empty.");
                            client.publish_diagnostics(uri, vec![], Some(version)).await;
                        } else if open_files.contains(&uri) {
                            tracing::info!(uri = uri.to_string(), "sent {} cargo diagnostics", diagnostics.len());
                            client.publish_diagnostics(uri, diagnostics, Some(version)).await;
                        }
                    }

                    progress.report(100).await;
                    progress.finish().await;
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

//! Bacon Language Server
use std::borrow::Cow;
use std::collections::HashSet;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use argh::FromArgs;
use bacon::Bacon;
use ls_types::{MessageType, ProgressToken, Uri, WorkspaceFolder};
use native::Cargo;
use rand::RngExt;
use serde_json::{Map, Value};
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tower_lsp_server::{Client, LspService, Server, jsonrpc};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CargoState {
    Idle,
    Running,
    RunningPending,
}

#[derive(Debug, Clone)]
pub(crate) enum PublishMode {
    CancelRunning,
    QueueIfRunning(CargoState),
}

#[derive(Debug)]
pub(crate) struct CargoOptions {
    pub(crate) command_args: String,
    pub(crate) env: Vec<String>,
    pub(crate) update_on_change: bool,
    pub(crate) update_on_change_cooldown: Duration,
    pub(crate) publish_mode: PublishMode,
}

impl CargoOptions {
    pub(crate) fn update_from_json_obj(&mut self, cargo_obj: &Map<String, Value>) -> jsonrpc::Result<()> {
        if let Some(value) = cargo_obj.get("commandArguments") {
            self.command_args = value
                .as_str()
                .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidParams))?
                .to_string();
        }
        if let Some(value) = cargo_obj.get("env") {
            self.env = value
                .as_str()
                .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidParams))?
                .split(',')
                .map(|x| x.trim().to_owned())
                .collect::<Vec<_>>();
        }
        if let Some(value) = cargo_obj.get("cancelRunning") {
            let cancel = value
                .as_bool()
                .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidParams))?;
            self.publish_mode = if cancel {
                PublishMode::CancelRunning
            } else {
                PublishMode::QueueIfRunning(CargoState::Idle)
            };
        }
        if let Some(value) = cargo_obj.get("updateOnChange") {
            self.update_on_change = value
                .as_bool()
                .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidParams))?;
        }
        if let Some(value) = cargo_obj.get("updateOnChangeCooldownMillis") {
            self.update_on_change_cooldown = Duration::from_millis(
                value
                    .as_u64()
                    .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidParams))?,
            );
        }

        Ok(())
    }
}

impl Default for CargoOptions {
    fn default() -> Self {
        Self {
            command_args: CARGO_COMMAND_ARGS.to_string(),
            env: vec![],
            update_on_change: false,
            update_on_change_cooldown: Duration::from_millis(5000),
            publish_mode: PublishMode::CancelRunning,
        }
    }
}

#[derive(Debug)]
pub(crate) struct BaconOptions {
    pub(crate) locations_file: String,
    pub(crate) run_in_background: bool,
    pub(crate) run_in_background_command: String,
    pub(crate) run_in_background_command_args: String,
    pub(crate) validate_preferences: bool,
    pub(crate) create_preferences_file: bool,
    pub(crate) synchronize_all_open_files_wait: Duration,
    pub(crate) update_on_save: bool,
    pub(crate) update_on_save_wait: Duration,
}

impl BaconOptions {
    pub(crate) fn update_from_json_obj(&mut self, bacon_obj: &Map<String, Value>) -> jsonrpc::Result<()> {
        if let Some(value) = bacon_obj.get("locationsFile") {
            self.locations_file = value
                .as_str()
                .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidParams))?
                .to_string();
        }
        if let Some(value) = bacon_obj.get("runInBackground") {
            self.run_in_background = value
                .as_bool()
                .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidParams))?;
        }
        if let Some(value) = bacon_obj.get("runInBackgroundCommand") {
            self.run_in_background_command = value
                .as_str()
                .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidParams))?
                .to_string();
        }
        if let Some(value) = bacon_obj.get("runInBackgroundCommandArguments") {
            self.run_in_background_command_args = value
                .as_str()
                .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidParams))?
                .to_string();
        }
        if let Some(value) = bacon_obj.get("validatePreferences") {
            self.validate_preferences = value
                .as_bool()
                .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidParams))?;
        }
        if let Some(value) = bacon_obj.get("createPreferencesFile") {
            self.create_preferences_file = value
                .as_bool()
                .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidParams))?;
        }
        if let Some(value) = bacon_obj.get("synchronizeAllOpenFilesWaitMillis") {
            self.synchronize_all_open_files_wait = Duration::from_millis(
                value
                    .as_u64()
                    .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidParams))?,
            );
        }
        if let Some(value) = bacon_obj.get("updateOnSave") {
            self.update_on_save = value
                .as_bool()
                .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidParams))?;
        }
        if let Some(value) = bacon_obj.get("updateOnSaveWaitMillis") {
            self.update_on_save_wait = Duration::from_millis(
                value
                    .as_u64()
                    .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidParams))?,
            );
        }

        Ok(())
    }
}

impl Default for BaconOptions {
    fn default() -> Self {
        Self {
            locations_file: LOCATIONS_FILE.to_string(),
            run_in_background: true,
            run_in_background_command: BACON_BACKGROUND_COMMAND.to_string(),
            run_in_background_command_args: BACON_BACKGROUND_COMMAND_ARGS.to_string(),
            validate_preferences: true,
            create_preferences_file: true,
            synchronize_all_open_files_wait: Duration::from_millis(2000),
            update_on_save: true,
            update_on_save_wait: Duration::from_millis(1000),
        }
    }
}

#[derive(Debug)]
struct State {
    project_root: Option<PathBuf>,
    workspace_folders: Option<Vec<WorkspaceFolder>>,
    bacon_command_handle: Option<JoinHandle<()>>,
    diagnostics_data_supported: bool,
    open_files: HashSet<Uri>,
    cancel_token: CancellationToken,
    sync_files_handle: Option<JoinHandle<()>>,
    backend: Backend,
    diagnostics_version: i32,
    build_folder: PathBuf,
    last_change: Instant,
    cargo: CargoOptions,
    bacon: BaconOptions,
}

impl Default for State {
    fn default() -> Self {
        Self {
            project_root: None,
            workspace_folders: None,
            bacon_command_handle: None,
            diagnostics_data_supported: false,
            open_files: HashSet::new(),
            cancel_token: CancellationToken::new(),
            sync_files_handle: None,
            backend: Backend::Cargo,
            diagnostics_version: 0,
            build_folder: tempfile::tempdir().unwrap().path().into(),
            last_change: Instant::now(),
            cargo: CargoOptions::default(),
            bacon: BaconOptions::default(),
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

    async fn pull_configuration(&self) {
        tracing::info!("pull_configuration");
        let Some(client) = self.client.as_ref() else {
            tracing::error!("no client to pull config from");
            return;
        };

        let response = match client
            .configuration(vec![ls_types::ConfigurationItem {
                scope_uri: None,
                section: Some("bacon_ls".to_string()),
            }])
            .await
        {
            Ok(response) => response,
            Err(e) => {
                tracing::error!("failed to pull configuration: {e}");
                return;
            }
        };

        let Some(settings) = response.into_iter().next() else {
            tracing::warn!("empty configuration response from client");
            return;
        };

        tracing::info!("pulled configuration: {settings:#?}");

        let mut state = self.state.write().await;
        if let Some(values) = settings.as_object() {
            if let Some(value) = values.get("useBaconBackend") {
                if let Some(use_bacon) = value.as_bool() {
                    state.backend = if use_bacon { Backend::Bacon } else { Backend::Cargo };
                }
            }
            if let Some(cargo_obj) = values.get("cargo").and_then(|v| v.as_object()) {
                if let Err(e) = state.cargo.update_from_json_obj(cargo_obj) {
                    tracing::error!("invalid cargo configuration: {e}");
                    client
                        .show_message(MessageType::ERROR, format!("Error in \"cargo\" section: {e}"))
                        .await;
                }
            }
            if let Some(bacon_obj) = values.get("bacon").and_then(|v| v.as_object()) {
                if let Err(e) = state.bacon.update_from_json_obj(bacon_obj) {
                    tracing::error!("invalid bacon configuration: {e}");
                    client
                        .show_message(MessageType::ERROR, format!("Error in \"bacon\" section: {e}"))
                        .await;
                }
            }
        }

        if let Backend::Cargo = state.backend {
            if !state.cargo.update_on_change {
                if let Some(root) = &state.project_root {
                    state.build_folder = root.clone();
                }
            }
        }
        tracing::debug!("configuration after pull: {state:#?}");
    }

    async fn publish_diagnostics(&self, uri: &Uri) {
        let mut guard = self.state.write().await;
        let locations_file_name = guard.bacon.locations_file.clone();
        let workspace_folders = guard.workspace_folders.clone();
        let open_files = guard.open_files.clone();
        let backend = guard.backend;
        let command_args = guard.cargo.command_args.clone();
        let cargo_env = guard.cargo.env.clone();
        let project_root = guard.project_root.clone();
        let build_folder = guard.build_folder.clone();
        guard.diagnostics_version += 1;
        let version = guard.diagnostics_version;

        let cancel_token = if let Backend::Cargo = backend {
            match &mut guard.cargo.publish_mode {
                PublishMode::CancelRunning => {
                    guard.cancel_token.cancel();
                    guard.cancel_token = CancellationToken::new();
                    Some(guard.cancel_token.clone())
                }
                PublishMode::QueueIfRunning(cargo_state) => match cargo_state {
                    CargoState::Running | CargoState::RunningPending => {
                        *cargo_state = CargoState::RunningPending;
                        tracing::debug!("cargo already running, marking pending");
                        drop(guard);
                        return;
                    }
                    CargoState::Idle => {
                        *cargo_state = CargoState::Running;
                        Some(guard.cancel_token.clone())
                    }
                },
            }
        } else {
            None
        };
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

                    let cancel_token = cancel_token.expect("cancel_token set for Cargo backend");
                    let cargo_future =
                        Cargo::cargo_diagnostics(&command_args, &cargo_env, project_root.as_ref(), &build_folder);

                    let diagnostics = tokio::select! {
                        result = cargo_future => {
                            result
                                .inspect_err(|err| tracing::error!(?err, "error building diagnostics"))
                                .unwrap_or_default()
                        }
                        () = cancel_token.cancelled() => {
                            tracing::info!("cargo run cancelled by newer request");
                            progress.finish().await;
                            return;
                        }
                    };

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

                    let mut guard = self.state.write().await;
                    if let PublishMode::QueueIfRunning(cargo_state) = &mut guard.cargo.publish_mode {
                        match cargo_state {
                            CargoState::RunningPending => {
                                *cargo_state = CargoState::Running;
                                drop(guard);
                                tracing::info!("re-running cargo after queued request");
                                Box::pin(self.publish_diagnostics(uri)).await;
                            }
                            _ => {
                                *cargo_state = CargoState::Idle;
                                drop(guard);
                            }
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

    #[test]
    fn test_cancel_mode_replaces_token() {
        let original = CancellationToken::new();
        let token = original.clone();
        token.cancel();
        assert!(original.is_cancelled());
        let new_token = CancellationToken::new();
        assert!(!new_token.is_cancelled());
    }
}

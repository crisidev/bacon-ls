//! Bacon Language Server
use std::collections::{HashMap, HashSet};
use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use argh::FromArgs;
use bacon::Bacon;
use flume::RecvError;
use ls_types::{Diagnostic, DiagnosticSeverity, MessageType, ProgressToken, Range, Uri, WorkspaceFolder};
use native::Cargo;
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use serde_json::{Map, Value};
use tokio::sync::{RwLock, RwLockWriteGuard};
use tokio::task::JoinHandle;
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

// Characters that must be percent-encoded when putting an OS path into a
// `file://` URI. We keep `/` unencoded so it continues to split the path into
// segments (clients expect multi-segment URIs). This covers the reserved URI
// characters plus a few that break `Uri` parsing in practice (space, `#`,
// `?`, `%`, `[`/`]`, backslash, etc.).
const PATH_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}')
    .add(b'%');

/// Build a `file://...` URI string from an OS path. Percent-encodes any
/// characters that would otherwise break URI parsing (spaces, `#`, `?`, `%`,
/// etc.), while leaving `/` intact so path segments survive.
pub(crate) fn path_to_file_uri(path: &str) -> String {
    format!("file://{}", utf8_percent_encode(path, PATH_ENCODE_SET))
}

/// Hash key for deduplicating diagnostics that share the same range, severity,
/// and message. `DiagnosticSeverity` is `Eq` but not `Hash` in `ls-types`, so we
/// project it down to a small integer tag.
pub(crate) type DiagKey = (Range, i32, String);

pub(crate) fn diag_key(d: &Diagnostic) -> DiagKey {
    (d.range, severity_tag(d.severity), d.message.clone())
}

fn severity_tag(s: Option<DiagnosticSeverity>) -> i32 {
    match s {
        None => 0,
        Some(s) if s == DiagnosticSeverity::ERROR => 1,
        Some(s) if s == DiagnosticSeverity::WARNING => 2,
        Some(s) if s == DiagnosticSeverity::INFORMATION => 3,
        Some(s) if s == DiagnosticSeverity::HINT => 4,
        Some(_) => -1,
    }
}

/// bacon-ls - https://github.com/crisidev/bacon-ls
#[derive(Debug, FromArgs)]
pub struct Args {
    /// display version information
    #[argh(switch, short = 'v')]
    pub version: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackendChoice {
    Cargo,
    Bacon,
}

#[derive(Debug)]
enum BackendRuntime {
    Bacon {
        config: BaconOptions,
        runtime: BaconRuntime,
    },
    Cargo {
        config: CargoOptions,
        runtime: CargoRuntime,
    },
}

impl BackendRuntime {
    fn backend_choice(&self) -> BackendChoice {
        match self {
            Self::Bacon { .. } => BackendChoice::Bacon,
            Self::Cargo { .. } => BackendChoice::Cargo,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CargoRunState {
    Idle,
    Running,
    RunningPending,
}

#[derive(Debug, Copy, Clone)]
pub(crate) enum PublishMode {
    CancelRunning,
    QueueIfRunning,
}

#[derive(Debug)]
pub(crate) struct CargoOptions {
    // "check" or "clippy"
    pub(crate) command: String,
    pub(crate) features: Vec<String>,
    // `-p crate_name`
    pub(crate) package: Option<String>,
    // Extra arguments which do not have a nice wrapper
    pub(crate) extra_command_args: Vec<String>,
    pub(crate) env: Vec<(String, String)>,
    pub(crate) publish_mode: PublishMode,
    // Interval at which we refresh (send) cargo diagnostics we have so far
    // None means wait until the cargo command is fully done
    pub(crate) refresh_interval_seconds: Option<Duration>,
    /// User override: when `Some(true)`, always emit children as separate
    /// diagnostics instead of related information, regardless of client
    /// capability. When `None`, follow the client advertisement.
    pub(crate) separate_child_diagnostics: Option<bool>,
    pub(crate) check_on_save: bool,
    pub(crate) clear_diagnostics_on_check: bool,
}

impl CargoOptions {
    pub(crate) fn build_command_args(&self) -> Vec<String> {
        let mut args = vec![self.command.clone()];
        args.push("--message-format=json-diagnostic-rendered-ansi".to_string());

        if !self.features.is_empty() {
            args.push("--features".to_string());
            let mut features = String::new();
            for feature in &self.features[..self.features.len() - 1] {
                features += feature;
                features += ",";
            }
            features += &self.features[self.features.len() - 1];
            args.push(features);
        }

        if let Some(pkg) = self.package.clone() {
            args.push("-p".to_string());
            args.push(pkg);
        }

        for arg in self.extra_command_args.iter().cloned() {
            args.push(arg);
        }

        args
    }

    pub(crate) fn update_from_json_obj(&mut self, cargo_obj: &Map<String, Value>) -> jsonrpc::Result<()> {
        if let Some(value) = cargo_obj.get("command") {
            self.command = value
                .as_str()
                .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidParams))?
                .to_string();
        }

        if let Some(value) = cargo_obj.get("features") {
            self.features = value
                .as_array()
                .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidParams))?
                .iter()
                .map(|item| {
                    item.as_str()
                        .map(|s| s.to_string())
                        .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidParams))
                })
                .collect::<jsonrpc::Result<Vec<_>>>()?;
        }

        if let Some(value) = cargo_obj.get("package") {
            self.package = Some(
                value
                    .as_str()
                    .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidParams))?
                    .to_string(),
            );
        }

        if let Some(value) = cargo_obj.get("extraArgs") {
            self.extra_command_args = value
                .as_array()
                .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidParams))?
                .iter()
                .map(|item| {
                    item.as_str()
                        .map(|s| s.to_string())
                        .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidParams))
                })
                .collect::<jsonrpc::Result<Vec<_>>>()?;
        }

        if let Some(value) = cargo_obj.get("env") {
            self.env = value
                .as_object()
                .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidParams))?
                .iter()
                .map(|(k, v)| {
                    let val = v
                        .as_str()
                        .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidParams))?;
                    Ok((k.clone(), val.to_string()))
                })
                .collect::<jsonrpc::Result<Vec<_>>>()?;
        }

        if let Some(value) = cargo_obj.get("cancelRunning") {
            let cancel = value
                .as_bool()
                .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidParams))?;
            self.publish_mode = if cancel {
                PublishMode::CancelRunning
            } else {
                PublishMode::QueueIfRunning
            };
        }

        if let Some(value) = cargo_obj.get("refreshIntervalSeconds") {
            if value.is_null() {
                self.refresh_interval_seconds = None;
            } else {
                let seconds = value
                    .as_i64()
                    .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidParams))?;
                if seconds < 0 {
                    self.refresh_interval_seconds = None;
                } else {
                    self.refresh_interval_seconds = Some(Duration::from_secs(seconds as u64));
                }
            }
        }

        if let Some(value) = cargo_obj.get("separateChildDiagnostics") {
            self.separate_child_diagnostics = value.as_bool();
        }

        if let Some(value) = cargo_obj.get("checkOnSave") {
            self.check_on_save = value
                .as_bool()
                .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidParams))?;
        }

        if let Some(value) = cargo_obj.get("clearDiagnosticsOnCheck") {
            self.clear_diagnostics_on_check = value
                .as_bool()
                .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::InvalidParams))?;
        }

        Ok(())
    }

    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }
}

impl Default for CargoOptions {
    fn default() -> Self {
        Self {
            env: Vec::new(),
            publish_mode: PublishMode::CancelRunning,
            command: "check".to_string(),
            features: vec![],
            extra_command_args: vec![],
            package: None,
            refresh_interval_seconds: Some(Duration::from_secs(5)),
            separate_child_diagnostics: None,
            check_on_save: true,
            clear_diagnostics_on_check: false,
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

    pub fn reset(&mut self) {
        *self = Self::default();
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
pub(crate) struct CargoRuntime {
    cancel_token: CancellationToken,
    run_state: CargoRunState,
    files_with_diags: HashSet<Uri>,
    diagnostics_version: i32,
    build_folder: PathBuf,
    // Timestamp of the most recent publish_cargo_diagnostics invocation.
    // Used by did_open to avoid kicking off a redundant run when one was
    // just triggered (e.g. the initial run from `initialized` immediately
    // followed by the client's first `didOpen`).
    last_run_started: Option<Instant>,
}

impl Default for CargoRuntime {
    fn default() -> Self {
        Self {
            cancel_token: CancellationToken::new(),
            run_state: CargoRunState::Idle,
            files_with_diags: HashSet::new(),
            diagnostics_version: 0,
            build_folder: PathBuf::new(),
            last_run_started: None,
        }
    }
}

#[derive(Debug)]
pub(crate) struct BaconRuntime {
    pub(crate) shutdown_token: CancellationToken,
    pub(crate) open_files: HashSet<Uri>,
    // Some(..) if we have to run bacon in the background ourselves
    pub(crate) command_handle: Option<JoinHandle<()>>,
    pub(crate) sync_files_handle: JoinHandle<()>,
    // Monotonic counter stamped onto each publishDiagnostics call so clients
    // can discard stale results if publishes arrive out of order.
    pub(crate) diagnostics_version: i32,
}

#[derive(Debug, Default)]
struct State {
    project_root: Option<PathBuf>,
    workspace_folders: Option<Vec<WorkspaceFolder>>,
    diagnostics_data_supported: bool,
    related_information_supported: bool,
    backend: Option<BackendRuntime>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct CorrectionEdit {
    pub(crate) range: Range,
    pub(crate) new_text: String,
}

// A single logical fix can require several disjoint byte-range edits. For
// example, removing `Compact` from `use …::{Compact, FmtSpan}` produces three
// edits: remove `{`, remove `Compact, `, remove `}`, leaving `use …::FmtSpan`.
// All edits must be applied atomically so the file stays valid.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct Correction {
    pub(crate) label: String,
    pub(crate) edits: Vec<CorrectionEdit>,
}

impl Correction {
    pub(crate) fn from_single(range: Range, new_text: &str) -> Self {
        let label = if new_text.is_empty() {
            "Remove".to_string()
        } else {
            format!("Replace with: {new_text}")
        };
        Self {
            label,
            edits: vec![CorrectionEdit {
                range,
                new_text: new_text.to_string(),
            }],
        }
    }

    pub(crate) fn from_multi(edits: Vec<CorrectionEdit>) -> Self {
        let label = match edits.iter().find(|e| !e.new_text.is_empty()) {
            None => "Remove".to_string(),
            Some(e) => format!("Replace with: {}", e.new_text),
        };
        Self { label, edits }
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct DiagnosticData {
    corrections: Vec<Correction>,
}

#[derive(Debug)]
pub struct BaconLs {
    client: Arc<Client>,
    state: Arc<RwLock<State>>,
}

impl BaconLs {
    fn new(client: Client) -> Self {
        Self {
            client: Arc::new(client),
            state: Arc::new(RwLock::new(State::default())),
        }
    }

    fn configure_tracing(log_level: Option<String>) {
        // Configure logging to file.
        let level = log_level.unwrap_or_else(|| env::var("RUST_LOG").unwrap_or("off".to_string()));
        if level == "off" {
            return;
        }
        let log_path = format!("{PKG_NAME}.log");
        let file = match std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&log_path)
        {
            Ok(file) => file,
            Err(e) => {
                // stdin/stdout are the LSP jsonrpc pipes; stderr is usually
                // captured by the client's trace window. One line there is the
                // best we can do to tell the user why logging is silent.
                eprintln!("{PKG_NAME}: could not open log file {log_path}: {e} (tracing disabled)");
                return;
            }
        };
        tracing_subscriber::fmt()
            .with_env_filter(level)
            .with_writer(file)
            .with_thread_names(true)
            .with_span_events(FmtSpan::CLOSE)
            .with_target(true)
            .with_file(true)
            .with_line_number(true)
            .init();
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
        // Force the process to terminate instead of waiting for the tokio
        // runtime to drain. Some background tasks (bacon subprocess readers,
        // file watchers) can linger past the `exit` notification; if the
        // process doesn't die promptly, `:LspRestart` in Neovim gives up
        // before starting a fresh instance.
        std::process::exit(0);
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

    fn detect_backend(values: &Map<String, Value>) -> Result<BackendChoice, String> {
        if let Some(value) = values.get("backend") {
            let backend = value.as_str().ok_or("'backend' must be a string")?;
            match backend {
                "cargo" => Ok(BackendChoice::Cargo),
                "bacon" => Ok(BackendChoice::Bacon),
                other => Err(format!("Invalid backend value '{other}'. Must be 'cargo' or 'bacon'.")),
            }
        } else {
            let has_cargo = values.get("cargo").and_then(|v| v.as_object()).is_some();
            let has_bacon = values.get("bacon").and_then(|v| v.as_object()).is_some();
            match (has_cargo, has_bacon) {
                (true, true) => Err(
                    "Both 'cargo' and 'bacon' config sections present without a 'backend' key. \
                     Set 'backend' to 'cargo' or 'bacon'."
                        .to_string(),
                ),
                (_, true) => Ok(BackendChoice::Bacon),
                _ => Ok(BackendChoice::Cargo),
            }
        }
    }

    async fn pull_configuration(&self) {
        tracing::debug!("pull_configuration");

        let configuration_fut = self.client.configuration(vec![ls_types::ConfigurationItem {
            scope_uri: None,
            section: Some("bacon_ls".to_string()),
        }]);
        // A client that never answers `workspace/configuration` (e.g. one
        // mid-teardown) would otherwise keep this await alive forever, which
        // in turn pins the `initialized` future inside the server loop and
        // blocks a clean shutdown.
        let response = match tokio::time::timeout(std::time::Duration::from_secs(5), configuration_fut).await {
            Ok(Ok(response)) => response,
            Ok(Err(e)) => {
                tracing::error!("failed to pull configuration: {e}");
                return;
            }
            Err(_) => {
                tracing::warn!("workspace/configuration request timed out; proceeding with defaults");
                return;
            }
        };

        let Some(settings) = response.into_iter().next() else {
            tracing::warn!("empty configuration response from client");
            return;
        };

        tracing::trace!("pulled configuration: {settings:#?}");
        self.adapt_to_settings(&settings).await;
    }

    async fn adapt_to_settings(&self, settings: &Value) {
        let mut state = self.state.write().await;
        let Some(values) = settings.as_object() else {
            tracing::warn!("configuration is not a JSON object");
            return;
        };

        if state.backend.is_none() {
            let backend_choice = match Self::detect_backend(values) {
                Ok(choice) => {
                    tracing::info!(backend = ?choice, "backend detected");
                    choice
                }
                Err(msg) => {
                    tracing::error!("{msg}");
                    self.client.show_message(MessageType::ERROR, &msg).await;
                    return;
                }
            };

            match backend_choice {
                BackendChoice::Bacon => {
                    let mut config = BaconOptions::default();
                    if let Some(bacon_obj) = values.get("bacon").and_then(|v| v.as_object())
                        && let Err(e) = config.update_from_json_obj(bacon_obj)
                    {
                        tracing::error!("invalid bacon configuration: {e}");
                        self.client
                            .show_message(MessageType::ERROR, format!("Error in \"bacon\" section: {e}"))
                            .await;
                    }

                    if config.validate_preferences {
                        if let Err(e) = Bacon::validate_preferences(
                            &config.run_in_background_command,
                            config.create_preferences_file,
                        )
                        .await
                        {
                            tracing::error!("{e}");
                            self.client.show_message(MessageType::ERROR, e).await;
                        }
                    } else {
                        tracing::warn!("skipping validation of bacon preferences, validateBaconPreferences is false");
                    }

                    let proj_root = state.project_root.clone();
                    let shutdown_token = CancellationToken::new();
                    let command_handle = if config.run_in_background {
                        let mut current_dir = None;
                        if let Ok(cwd) = env::current_dir() {
                            current_dir = Self::find_git_root_directory(&cwd).await;
                            if let Some(dir) = &current_dir {
                                if !dir.join("Cargo.toml").exists() {
                                    current_dir = proj_root;
                                }
                            } else {
                                current_dir = proj_root;
                            }
                        }

                        match Bacon::run_in_background(
                            &config.run_in_background_command,
                            &config.run_in_background_command_args,
                            current_dir.as_ref(),
                            shutdown_token.clone(),
                        )
                        .await
                        {
                            Ok(command) => {
                                tracing::info!("bacon was started successfully and is running in the background");
                                Some(command)
                            }
                            Err(e) => {
                                tracing::error!("{e}");
                                self.client.show_message(MessageType::ERROR, e).await;
                                None
                            }
                        }
                    } else {
                        tracing::warn!("skipping background bacon startup, runBaconInBackground is false");
                        None
                    };

                    let task_state = self.state.clone();
                    let task_client = self.client.clone();
                    state.backend = Some(BackendRuntime::Bacon {
                        config,
                        runtime: BaconRuntime {
                            shutdown_token,
                            open_files: HashSet::new(),
                            command_handle,
                            sync_files_handle: tokio::task::spawn(Self::synchronize_diagnostics(
                                task_state,
                                task_client,
                            )),
                            diagnostics_version: 0,
                        },
                    });
                    tracing::info!("bacon backend initialized");
                }
                BackendChoice::Cargo => {
                    let mut config = CargoOptions::default();
                    if let Some(cargo_obj) = values.get("cargo").and_then(|v| v.as_object())
                        && let Err(e) = config.update_from_json_obj(cargo_obj)
                    {
                        tracing::error!("invalid cargo configuration: {e}");
                        self.client
                            .show_message(MessageType::ERROR, format!("Error in \"cargo\" section: {e}"))
                            .await;
                    }
                    if let Err(e) = Self::init_cargo_backend(&mut state, config) {
                        tracing::error!("{e}");
                        drop(state);
                        self.client.show_message(MessageType::ERROR, e).await;
                        return;
                    }
                    drop(state);
                }
            }
        } else {
            let current_choice = match &state.backend {
                Some(BackendRuntime::Bacon { .. }) => BackendChoice::Bacon,
                Some(BackendRuntime::Cargo { .. }) => BackendChoice::Cargo,
                None => unreachable!("backend is Some in this branch"),
            };
            let desired = match Self::detect_backend(values) {
                Ok(choice) => choice,
                Err(err) => {
                    tracing::error!("invalid backend configuration on reload: {err}");
                    self.client.show_message(MessageType::ERROR, &err).await;
                    return;
                }
            };

            if desired != current_choice {
                let msg = "Backend cannot be changed while the server is running. \
                           Restart the server to switch backends.";
                tracing::error!("{msg}");
                self.client.show_message(MessageType::ERROR, msg).await;
                return;
            }

            let project_root = state.project_root.clone();
            match &mut state.backend {
                Some(BackendRuntime::Cargo { config, runtime }) => {
                    config.reset();
                    if let Some(cargo_obj) = values.get("cargo").and_then(|v| v.as_object())
                        && let Err(e) = config.update_from_json_obj(cargo_obj)
                    {
                        tracing::error!("invalid cargo configuration: {e}");
                        self.client
                            .show_message(MessageType::ERROR, format!("Error in \"cargo\" section: {e}"))
                            .await;
                    }
                    if let Some(root) = project_root {
                        runtime.build_folder = root;
                    }
                    tracing::debug!("cargo configuration updated");
                }
                Some(BackendRuntime::Bacon { config, .. }) => {
                    config.reset();
                    if let Some(bacon_obj) = values.get("bacon").and_then(|v| v.as_object())
                        && let Err(e) = config.update_from_json_obj(bacon_obj)
                    {
                        tracing::error!("invalid bacon configuration: {e}");
                        self.client
                            .show_message(MessageType::ERROR, format!("Error in \"bacon\" section: {e}"))
                            .await;
                    }
                    tracing::debug!("bacon configuration updated");
                }
                None => unreachable!("backend is Some in this branch"),
            }
        }
    }

    fn init_cargo_backend(state: &mut RwLockWriteGuard<'_, State>, config: CargoOptions) -> Result<(), String> {
        let build_folder = match &state.project_root {
            Some(root) => root.clone(),
            None => match env::current_dir() {
                Ok(cwd) => {
                    tracing::warn!(
                        "no Cargo project root detected; falling back to current working directory: {}",
                        cwd.display()
                    );
                    cwd
                }
                Err(e) => {
                    return Err(format!(
                        "cargo backend cannot start: no project root detected and current working \
                         directory is unavailable ({e}). Open a folder containing a Cargo.toml and \
                         restart the server."
                    ));
                }
            },
        };
        let runtime = CargoRuntime {
            build_folder,
            ..CargoRuntime::default()
        };
        tracing::info!(build_folder = ?runtime.build_folder, "cargo backend initialized");
        state.backend = Some(BackendRuntime::Cargo { config, runtime });
        Ok(())
    }

    async fn publish_cargo_diagnostics(&self) {
        tracing::info!("starting cargo diagnostics run");
        let mut guard = self.state.write().await;
        let project_root = guard.project_root.clone();
        let related_information_supported = guard.related_information_supported;

        let Some(BackendRuntime::Cargo { config, runtime }) = &mut guard.backend else {
            return;
        };
        let use_related_information = !config
            .separate_child_diagnostics
            .unwrap_or(!related_information_supported);
        let cargo_command = config.command.clone();
        let cargo_env = config.env.clone();
        let cmd_args = config.build_command_args();
        let publish_mode = config.publish_mode;
        let clear_diagnostics_on_check = config.clear_diagnostics_on_check;
        let build_folder = runtime.build_folder.clone();
        runtime.diagnostics_version += 1;
        runtime.last_run_started = Some(Instant::now());
        let version = runtime.diagnostics_version;
        let refresh_interval = config.refresh_interval_seconds;

        let cancel_token = match publish_mode {
            PublishMode::CancelRunning => {
                runtime.cancel_token.cancel();
                runtime.cancel_token = CancellationToken::new();
                runtime.cancel_token.clone()
            }
            PublishMode::QueueIfRunning => match runtime.run_state {
                CargoRunState::Running | CargoRunState::RunningPending => {
                    runtime.run_state = CargoRunState::RunningPending;
                    tracing::debug!("cargo already running, marking pending");
                    drop(guard);
                    return;
                }
                CargoRunState::Idle => {
                    runtime.run_state = CargoRunState::Running;
                    runtime.cancel_token.clone()
                }
            },
        };

        if clear_diagnostics_on_check {
            for file in &runtime.files_with_diags {
                self.client
                    .publish_diagnostics(file.clone(), vec![], Some(version))
                    .await;
            }
            runtime.files_with_diags.clear();
        }

        drop(guard);

        let token = ProgressToken::Number(version);
        let progress = self
            .client
            .progress(token, "checking")
            .with_message(format!("cargo {cargo_command}"))
            .with_percentage(0)
            .begin()
            .await;

        let (tx, rx) = flume::unbounded();

        let cargo_future = Cargo::cargo_diagnostics(
            cmd_args,
            &cargo_env,
            project_root.as_ref(),
            &build_folder,
            use_related_information,
            &progress,
            tx,
        );

        let consumer_client = self.client.clone();
        let diagnostic_consumer = async move {
            // Per-URI bucket: the diagnostics to publish, a `seen` set keyed by
            // (range, severity, message) for O(1) dedup, and a dirty flag for
            // partial publishes during the cargo run.
            let mut diagnostics_map = HashMap::<Uri, (Vec<Diagnostic>, HashSet<DiagKey>, bool)>::new();

            fn accumulate_diagnostics(
                recv_result: Result<(Uri, Diagnostic), RecvError>,
                diagnostics_map: &mut HashMap<Uri, (Vec<Diagnostic>, HashSet<DiagKey>, bool)>,
            ) -> bool {
                let Ok((url, diagnostic)) = recv_result else {
                    return true;
                };
                let (diagnostics, seen, dirty) = diagnostics_map.entry(url).or_default();
                if seen.insert(diag_key(&diagnostic)) {
                    diagnostics.push(diagnostic);
                    *dirty = true;
                }
                false
            }

            if let Some(refresh_interval) = refresh_interval {
                let mut t = std::time::Instant::now();
                loop {
                    tokio::select! {
                        result = rx.recv_async() => {
                            if accumulate_diagnostics(result, &mut diagnostics_map) {
                                break;
                            }
                        }
                        _ = tokio::time::sleep_until(tokio::time::Instant::from_std(t + refresh_interval)) => {}
                    }

                    if t.elapsed() >= refresh_interval {
                        for (url, (diagnostics, _seen, dirty)) in diagnostics_map.iter_mut() {
                            if *dirty {
                                consumer_client
                                    .publish_diagnostics(url.clone(), diagnostics.clone(), Some(version))
                                    .await;
                                *dirty = false;
                            }
                        }
                        t = std::time::Instant::now();
                    }
                }
            } else {
                loop {
                    if accumulate_diagnostics(rx.recv_async().await, &mut diagnostics_map) {
                        break;
                    }
                }
            }

            diagnostics_map
        };

        let consumer_handle = tokio::spawn(diagnostic_consumer);

        let result = tokio::select! {
            result = cargo_future => {
                result.map(|_| false)
            },
            () = cancel_token.cancelled() => {
                tracing::info!("cargo run cancelled by newer request");
                Ok(true)
            }
        };

        let was_cancelled = match result {
            Ok(t) => t,
            Err(error) => {
                // We know there wont be any diagnostics as they way we detect cargo errors is
                // if it exists with non 0 exit code and no diagnostics were found
                tracing::error!(?error, "error building diagnostics");
                progress.finish().await;
                let _ = consumer_handle.await;
                self.client.log_message(MessageType::ERROR, format!("{error}")).await;
                self.client.show_message(MessageType::ERROR, format!("{error}")).await;
                return;
            }
        };

        if was_cancelled {
            // The newer run that triggered cancellation owns publishing. Touching
            // files_with_diags or publishing partial results here would race with
            // it and could push stale diagnostics on top of correct ones.
            let _ = consumer_handle.await;
            progress.finish_with_message("cancelled by user").await;
            return;
        }

        tracing::info!("cargo run finished, collecting diagnostics");

        let mut diagnostics = match consumer_handle.await {
            Ok(d) => d,
            Err(error) => {
                tracing::error!(?error, "diagnostics fetching task panicked");
                progress.finish().await;
                self.client.log_message(MessageType::ERROR, format!("{error}")).await;
                self.client.show_message(MessageType::ERROR, format!("{error}")).await;
                return;
            }
        };

        let mut state = self.state.write().await;
        let Some(BackendRuntime::Cargo {
            config,
            runtime: cargo_rt,
        }) = &mut state.backend
        else {
            // This should be impossible to land here, if we do there a logic error
            tracing::error!("backend changed during cargo run");
            return;
        };
        let publish_mode = config.publish_mode;

        // In CancelRunning mode a newer run may have started after our cargo
        // process finished but before we reached this point. If so our results
        // are stale — skip publishing so we don't overwrite the newer run's
        // output with old data.
        if let PublishMode::CancelRunning = publish_mode
            && version != cargo_rt.diagnostics_version
        {
            tracing::info!(
                version,
                current = cargo_rt.diagnostics_version,
                "skipping stale publish"
            );
            progress.finish_with_message("superseded by newer run").await;
            return;
        }

        for file in cargo_rt.files_with_diags.drain() {
            // Add empty diagnostics so that it get cleared later
            let _ = diagnostics.entry(file).or_insert((vec![], HashSet::new(), true));
        }

        let mut num_warnings = 0;
        let mut num_errors = 0;
        for (uri, (diagnostics, _seen, is_dirty)) in diagnostics.into_iter() {
            tracing::debug!(uri = uri.to_string(), "sent {} cargo diagnostics", diagnostics.len());
            for diagnostic in &diagnostics {
                match diagnostic.severity {
                    Some(DiagnosticSeverity::ERROR) => num_errors += 1,
                    Some(DiagnosticSeverity::WARNING) => num_warnings += 1,
                    Some(_) | None => {}
                }
            }
            if !diagnostics.is_empty() {
                let _ = cargo_rt.files_with_diags.insert(uri.clone());
            }
            if is_dirty {
                self.client.publish_diagnostics(uri, diagnostics, Some(version)).await;
            }
        }
        let message = format!("done, errors: {num_errors}, warnings: {num_warnings}");
        progress.finish_with_message(message).await;

        if let PublishMode::QueueIfRunning = publish_mode {
            match cargo_rt.run_state {
                CargoRunState::RunningPending => {
                    cargo_rt.run_state = CargoRunState::Idle;
                    drop(state);
                    tracing::info!("re-running cargo after queued request");
                    Box::pin(self.publish_cargo_diagnostics()).await;
                }
                _ => {
                    cargo_rt.run_state = CargoRunState::Idle;
                    drop(state);
                }
            }
        }
    }

    async fn publish_bacon_diagnostics(&self, uri: &Uri) {
        let mut guard = self.state.write().await;
        let workspace_folders = guard.workspace_folders.clone();

        let Some(BackendRuntime::Bacon { config, runtime }) = &mut guard.backend else {
            return;
        };
        tracing::info!(uri = uri.to_string(), "publish bacon diagnostics");
        let locations_file_name = config.locations_file.clone();
        runtime.diagnostics_version = runtime.diagnostics_version.wrapping_add(1);
        let version = runtime.diagnostics_version;
        drop(guard);
        Bacon::publish_diagnostics(
            &self.client,
            uri,
            &locations_file_name,
            workspace_folders.as_deref(),
            version,
        )
        .await;
    }

    async fn synchronize_diagnostics(state: Arc<RwLock<State>>, client: Arc<Client>) {
        Bacon::synchronize_diagnostics(state, client).await;
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
    fn test_path_to_file_uri_plain_ascii() {
        let uri = path_to_file_uri("/home/me/src/lib.rs");
        assert_eq!(uri, "file:///home/me/src/lib.rs");
        let parsed = uri.parse::<Uri>().expect("must parse as Uri");
        assert_eq!(parsed.path().as_str(), "/home/me/src/lib.rs");
    }

    #[test]
    fn test_path_to_file_uri_escapes_space_and_hash_and_percent() {
        let uri = path_to_file_uri("/home/me/My Projects/tests#1/file%.rs");
        assert_eq!(uri, "file:///home/me/My%20Projects/tests%231/file%25.rs");
        let parsed = uri.parse::<Uri>().expect("must parse as Uri");
        // Uri preserves the encoded form on the wire; clients are responsible
        // for decoding. We only need to confirm the parse succeeds.
        assert_eq!(parsed.path().as_str(), "/home/me/My%20Projects/tests%231/file%25.rs");
    }

    #[test]
    fn test_path_to_file_uri_preserves_path_separators() {
        // The `/` separator must NOT be encoded, or clients can't recognize
        // segment structure.
        let uri = path_to_file_uri("/a/b/c");
        assert_eq!(uri, "file:///a/b/c");
    }

    #[test]
    fn test_path_to_file_uri_relative_path_preserves_segments() {
        // Cargo emits relative paths (e.g. "src/lib.rs") in JSON output. The
        // current `deserialize_url` hack turns those into URIs with the first
        // segment as "host" — percent-encoding must not break that.
        let uri = path_to_file_uri("src/lib.rs");
        assert_eq!(uri, "file://src/lib.rs");
        let parsed = uri.parse::<Uri>().expect("must parse as Uri");
        assert_eq!(
            parsed.authority().map(|a| a.host().to_string()),
            Some("src".to_string())
        );
        assert_eq!(parsed.path().as_str(), "/lib.rs");
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

    #[test]
    fn test_detect_backend_explicit_cargo() {
        let values: Map<String, Value> = serde_json::from_str(r#"{"backend": "cargo"}"#).unwrap();
        assert_eq!(BaconLs::detect_backend(&values).unwrap(), BackendChoice::Cargo);
    }

    #[test]
    fn test_detect_backend_explicit_bacon() {
        let values: Map<String, Value> = serde_json::from_str(r#"{"backend": "bacon"}"#).unwrap();
        assert_eq!(BaconLs::detect_backend(&values).unwrap(), BackendChoice::Bacon);
    }

    #[test]
    fn test_detect_backend_invalid_value() {
        let values: Map<String, Value> = serde_json::from_str(r#"{"backend": "invalid"}"#).unwrap();
        assert!(BaconLs::detect_backend(&values).is_err());
    }

    #[test]
    fn test_detect_backend_infer_from_cargo_key() {
        let values: Map<String, Value> = serde_json::from_str(r#"{"cargo": {"command": "check"}}"#).unwrap();
        assert_eq!(BaconLs::detect_backend(&values).unwrap(), BackendChoice::Cargo);
    }

    #[test]
    fn test_detect_backend_infer_from_bacon_key() {
        let values: Map<String, Value> =
            serde_json::from_str(r#"{"bacon": {"locationsFile": ".bacon-locations"}}"#).unwrap();
        assert_eq!(BaconLs::detect_backend(&values).unwrap(), BackendChoice::Bacon);
    }

    #[test]
    fn test_detect_backend_both_keys_error() {
        let values: Map<String, Value> = serde_json::from_str(r#"{"cargo": {}, "bacon": {}}"#).unwrap();
        assert!(BaconLs::detect_backend(&values).is_err());
    }

    #[test]
    fn test_detect_backend_no_keys_defaults_to_cargo() {
        let values: Map<String, Value> = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(BaconLs::detect_backend(&values).unwrap(), BackendChoice::Cargo);
    }

    #[test]
    fn test_detect_backend_explicit_overrides_keys() {
        let values: Map<String, Value> = serde_json::from_str(r#"{"backend": "cargo", "bacon": {}}"#).unwrap();
        assert_eq!(BaconLs::detect_backend(&values).unwrap(), BackendChoice::Cargo);
    }

    #[test]
    fn test_cargo_options_build_args_default() {
        let args = CargoOptions::default().build_command_args();
        assert_eq!(args, vec!["check", "--message-format=json-diagnostic-rendered-ansi"]);
    }

    #[test]
    fn test_cargo_options_build_args_with_features() {
        let opts = CargoOptions {
            features: vec!["a".into(), "b".into(), "c".into()],
            ..CargoOptions::default()
        };
        let args = opts.build_command_args();
        assert_eq!(
            args,
            vec![
                "check",
                "--message-format=json-diagnostic-rendered-ansi",
                "--features",
                "a,b,c"
            ]
        );
    }

    #[test]
    fn test_cargo_options_build_args_single_feature() {
        let opts = CargoOptions {
            features: vec!["only".into()],
            ..CargoOptions::default()
        };
        let args = opts.build_command_args();
        assert_eq!(
            args,
            vec![
                "check",
                "--message-format=json-diagnostic-rendered-ansi",
                "--features",
                "only"
            ]
        );
    }

    #[test]
    fn test_cargo_options_build_args_with_package_and_extras() {
        let opts = CargoOptions {
            command: "clippy".into(),
            package: Some("my-crate".into()),
            extra_command_args: vec!["--workspace".into(), "--all-targets".into()],
            ..CargoOptions::default()
        };
        let args = opts.build_command_args();
        assert_eq!(
            args,
            vec![
                "clippy",
                "--message-format=json-diagnostic-rendered-ansi",
                "-p",
                "my-crate",
                "--workspace",
                "--all-targets",
            ]
        );
    }

    #[test]
    fn test_cargo_options_update_from_json_full_roundtrip() {
        let mut opts = CargoOptions::default();
        let json = serde_json::json!({
            "command": "clippy",
            "features": ["a", "b"],
            "package": "pkg",
            "extraArgs": ["--workspace"],
            "env": {"RUST_LOG": "trace"},
            "cancelRunning": false,
            "refreshIntervalSeconds": 10,
            "separateChildDiagnostics": true,
            "checkOnSave": false,
            "clearDiagnosticsOnCheck": true,
        });
        let obj = json.as_object().unwrap();
        opts.update_from_json_obj(obj).expect("should parse");
        assert_eq!(opts.command, "clippy");
        assert_eq!(opts.features, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(opts.package.as_deref(), Some("pkg"));
        assert_eq!(opts.extra_command_args, vec!["--workspace".to_string()]);
        assert_eq!(opts.env, vec![("RUST_LOG".into(), "trace".into())]);
        assert!(matches!(opts.publish_mode, PublishMode::QueueIfRunning));
        assert_eq!(opts.refresh_interval_seconds, Some(Duration::from_secs(10)));
        assert_eq!(opts.separate_child_diagnostics, Some(true));
        assert!(!opts.check_on_save);
        assert!(opts.clear_diagnostics_on_check);
    }

    #[test]
    fn test_cargo_options_update_from_json_refresh_null_means_no_partial() {
        let mut opts = CargoOptions::default();
        let json = serde_json::json!({"refreshIntervalSeconds": null});
        opts.update_from_json_obj(json.as_object().unwrap()).unwrap();
        assert_eq!(opts.refresh_interval_seconds, None);
    }

    #[test]
    fn test_cargo_options_update_from_json_refresh_negative_means_no_partial() {
        let mut opts = CargoOptions::default();
        let json = serde_json::json!({"refreshIntervalSeconds": -1});
        opts.update_from_json_obj(json.as_object().unwrap()).unwrap();
        assert_eq!(opts.refresh_interval_seconds, None);
    }

    #[test]
    fn test_cargo_options_update_from_json_rejects_wrong_type() {
        let mut opts = CargoOptions::default();
        let json = serde_json::json!({"command": 42});
        assert!(opts.update_from_json_obj(json.as_object().unwrap()).is_err());
    }

    #[test]
    fn test_cargo_options_update_from_json_partial_leaves_others_unchanged() {
        let mut opts = CargoOptions {
            command: "clippy".into(),
            ..CargoOptions::default()
        };
        let json = serde_json::json!({"checkOnSave": false});
        opts.update_from_json_obj(json.as_object().unwrap()).unwrap();
        assert_eq!(opts.command, "clippy");
        assert!(!opts.check_on_save);
    }

    #[test]
    fn test_cargo_options_reset_restores_defaults() {
        let mut opts = CargoOptions {
            command: "clippy".into(),
            features: vec!["foo".into()],
            check_on_save: false,
            ..CargoOptions::default()
        };
        opts.reset();
        let defaults = CargoOptions::default();
        assert_eq!(opts.command, defaults.command);
        assert_eq!(opts.features, defaults.features);
        assert_eq!(opts.check_on_save, defaults.check_on_save);
    }

    #[test]
    fn test_bacon_options_update_from_json_full_roundtrip() {
        let mut opts = BaconOptions::default();
        let json = serde_json::json!({
            "locationsFile": "custom.locations",
            "runInBackground": false,
            "runInBackgroundCommand": "/usr/local/bin/bacon",
            "runInBackgroundCommandArguments": "--headless -j custom",
            "validatePreferences": false,
            "createPreferencesFile": false,
            "synchronizeAllOpenFilesWaitMillis": 500,
            "updateOnSave": false,
            "updateOnSaveWaitMillis": 250,
        });
        opts.update_from_json_obj(json.as_object().unwrap()).unwrap();
        assert_eq!(opts.locations_file, "custom.locations");
        assert!(!opts.run_in_background);
        assert_eq!(opts.run_in_background_command, "/usr/local/bin/bacon");
        assert_eq!(opts.run_in_background_command_args, "--headless -j custom");
        assert!(!opts.validate_preferences);
        assert!(!opts.create_preferences_file);
        assert_eq!(opts.synchronize_all_open_files_wait, Duration::from_millis(500));
        assert!(!opts.update_on_save);
        assert_eq!(opts.update_on_save_wait, Duration::from_millis(250));
    }

    #[test]
    fn test_bacon_options_update_from_json_rejects_wrong_type() {
        let mut opts = BaconOptions::default();
        let json = serde_json::json!({"runInBackground": "yes"});
        assert!(opts.update_from_json_obj(json.as_object().unwrap()).is_err());
    }

    #[test]
    fn test_bacon_options_reset_restores_defaults() {
        let mut opts = BaconOptions {
            run_in_background: false,
            locations_file: "foo".into(),
            ..BaconOptions::default()
        };
        opts.reset();
        let defaults = BaconOptions::default();
        assert_eq!(opts.run_in_background, defaults.run_in_background);
        assert_eq!(opts.locations_file, defaults.locations_file);
    }

    #[test]
    fn test_correction_from_single_empty_is_remove() {
        let range = Range::default();
        let c = Correction::from_single(range, "");
        assert_eq!(c.label, "Remove");
        assert_eq!(c.edits.len(), 1);
        assert_eq!(c.edits[0].new_text, "");
    }

    #[test]
    fn test_correction_from_single_nonempty_is_replace() {
        let range = Range::default();
        let c = Correction::from_single(range, "foo");
        assert_eq!(c.label, "Replace with: foo");
        assert_eq!(c.edits.len(), 1);
    }

    #[test]
    fn test_correction_from_multi_all_empty_is_remove() {
        let edits = vec![
            CorrectionEdit {
                range: Range::default(),
                new_text: "".into(),
            },
            CorrectionEdit {
                range: Range::default(),
                new_text: "".into(),
            },
        ];
        let c = Correction::from_multi(edits);
        assert_eq!(c.label, "Remove");
        assert_eq!(c.edits.len(), 2);
    }

    #[test]
    fn test_correction_from_multi_labels_by_first_nonempty() {
        let edits = vec![
            CorrectionEdit {
                range: Range::default(),
                new_text: "".into(),
            },
            CorrectionEdit {
                range: Range::default(),
                new_text: "new".into(),
            },
        ];
        let c = Correction::from_multi(edits);
        assert_eq!(c.label, "Replace with: new");
    }

    #[test]
    fn test_severity_tag_distinguishes_levels() {
        assert_eq!(severity_tag(None), 0);
        assert_eq!(severity_tag(Some(DiagnosticSeverity::ERROR)), 1);
        assert_eq!(severity_tag(Some(DiagnosticSeverity::WARNING)), 2);
        assert_eq!(severity_tag(Some(DiagnosticSeverity::INFORMATION)), 3);
        assert_eq!(severity_tag(Some(DiagnosticSeverity::HINT)), 4);
        // All four constants must hash to distinct tags or dedup will fold
        // legitimately-different diagnostics together.
        let tags = [
            severity_tag(Some(DiagnosticSeverity::ERROR)),
            severity_tag(Some(DiagnosticSeverity::WARNING)),
            severity_tag(Some(DiagnosticSeverity::INFORMATION)),
            severity_tag(Some(DiagnosticSeverity::HINT)),
        ];
        let unique: HashSet<_> = tags.iter().collect();
        assert_eq!(unique.len(), tags.len());
    }

    #[test]
    fn test_diag_key_collides_for_equal_diagnostics() {
        let a = Diagnostic {
            range: Range::default(),
            severity: Some(DiagnosticSeverity::ERROR),
            message: "hi".into(),
            ..Diagnostic::default()
        };
        let b = a.clone();
        assert_eq!(diag_key(&a), diag_key(&b));
    }

    #[test]
    fn test_diag_key_differs_when_message_differs() {
        let mut a = Diagnostic {
            range: Range::default(),
            severity: Some(DiagnosticSeverity::ERROR),
            message: "first".into(),
            ..Diagnostic::default()
        };
        let b = a.clone();
        a.message = "second".into();
        assert_ne!(diag_key(&a), diag_key(&b));
    }

    #[test]
    fn test_path_to_file_uri_empty_path() {
        // Empty path yields the trivial `file://` URI. Useful guard against
        // future regressions in the encoding helper when fed degenerate input.
        assert_eq!(path_to_file_uri(""), "file://");
    }

    #[test]
    fn test_correction_from_single_label_replaces_with_text() {
        let c = Correction::from_single(Range::default(), "x");
        assert_eq!(c.label, "Replace with: x");
        assert_eq!(c.edits.len(), 1);
        assert_eq!(c.edits[0].new_text, "x");
    }

    #[test]
    fn test_correction_from_multi_empty_edits_is_remove() {
        let c = Correction::from_multi(vec![]);
        assert_eq!(c.label, "Remove");
        assert!(c.edits.is_empty());
    }

    #[test]
    fn test_cargo_options_env_roundtrip_preserves_order_in_serde_iteration() {
        // serde_json::Map preserves insertion order. We rely on that for
        // reproducible env propagation into cargo.
        let mut opts = CargoOptions::default();
        let json = serde_json::json!({
            "env": {"A": "1", "B": "2", "C": "3"}
        });
        opts.update_from_json_obj(json.as_object().unwrap()).unwrap();
        assert_eq!(opts.env.len(), 3);
        let keys: Vec<_> = opts.env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["A", "B", "C"]);
    }

    #[test]
    fn test_cargo_options_update_rejects_non_object_env() {
        let mut opts = CargoOptions::default();
        let json = serde_json::json!({"env": ["A=1"]});
        assert!(opts.update_from_json_obj(json.as_object().unwrap()).is_err());
    }

    #[test]
    fn test_cargo_options_update_rejects_non_string_env_value() {
        let mut opts = CargoOptions::default();
        let json = serde_json::json!({"env": {"A": 1}});
        assert!(opts.update_from_json_obj(json.as_object().unwrap()).is_err());
    }

    #[test]
    fn test_cargo_options_update_rejects_non_string_feature_item() {
        let mut opts = CargoOptions::default();
        let json = serde_json::json!({"features": ["a", 2, "c"]});
        assert!(opts.update_from_json_obj(json.as_object().unwrap()).is_err());
    }

    #[test]
    fn test_cargo_options_publish_mode_toggle_via_cancel_running() {
        let mut opts = CargoOptions::default();
        // Default is CancelRunning.
        assert!(matches!(opts.publish_mode, PublishMode::CancelRunning));
        opts.update_from_json_obj(
            serde_json::json!({"cancelRunning": false}).as_object().unwrap(),
        )
        .unwrap();
        assert!(matches!(opts.publish_mode, PublishMode::QueueIfRunning));
        opts.update_from_json_obj(
            serde_json::json!({"cancelRunning": true}).as_object().unwrap(),
        )
        .unwrap();
        assert!(matches!(opts.publish_mode, PublishMode::CancelRunning));
    }

    #[test]
    fn test_cargo_options_separate_child_diagnostics_can_unset() {
        let mut opts = CargoOptions {
            separate_child_diagnostics: Some(true),
            ..CargoOptions::default()
        };
        // `as_bool()` on a non-bool returns None — and we feed that through
        // unchanged, so a `null` (or anything non-bool) clears the override.
        opts.update_from_json_obj(
            serde_json::json!({"separateChildDiagnostics": null}).as_object().unwrap(),
        )
        .unwrap();
        assert_eq!(opts.separate_child_diagnostics, None);
    }

    #[tokio::test]
    async fn test_find_git_root_directory_returns_none_outside_git() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = BaconLs::find_git_root_directory(tmp.path()).await;
        assert_eq!(root, None);
    }

    #[tokio::test]
    async fn test_find_git_root_directory_finds_top_of_repo() {
        // `git -C <subdir> rev-parse --show-toplevel` should resolve to the
        // crate's own repo root regardless of which subdirectory we point at.
        let crate_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let src = crate_root.join("src");
        let from_subdir = BaconLs::find_git_root_directory(&src).await;
        assert!(from_subdir.is_some(), "src/ is inside a git repo");
        let from_root = BaconLs::find_git_root_directory(crate_root).await.unwrap();
        // Both lookups should resolve to the same toplevel.
        assert_eq!(from_subdir.unwrap(), from_root);
    }

    #[test]
    fn test_init_cargo_backend_uses_existing_project_root() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let mut state = State {
            project_root: Some(root.clone()),
            ..State::default()
        };
        // Normally we'd hold an RwLockWriteGuard, but for this unit test we
        // adapt the API by going through a real lock.
        let lock = RwLock::new(std::mem::take(&mut state));
        let mut guard = lock.try_write().unwrap();
        BaconLs::init_cargo_backend(&mut guard, CargoOptions::default())
            .expect("init should succeed with explicit project root");
        match &guard.backend {
            Some(BackendRuntime::Cargo { runtime, .. }) => {
                assert_eq!(runtime.build_folder, root);
                assert_eq!(runtime.run_state, CargoRunState::Idle);
                assert_eq!(runtime.diagnostics_version, 0);
            }
            other => panic!("expected Cargo backend, got {other:?}"),
        }
    }

    #[test]
    fn test_init_cargo_backend_falls_back_to_cwd_when_no_project_root() {
        let mut state = State::default();
        let lock = RwLock::new(std::mem::take(&mut state));
        let mut guard = lock.try_write().unwrap();
        BaconLs::init_cargo_backend(&mut guard, CargoOptions::default())
            .expect("init should fall back to CWD when project root is unset");
        match &guard.backend {
            Some(BackendRuntime::Cargo { runtime, .. }) => {
                let cwd = std::env::current_dir().unwrap();
                assert_eq!(runtime.build_folder, cwd, "should fall back to CWD");
            }
            other => panic!("expected Cargo backend, got {other:?}"),
        }
    }

    #[test]
    fn test_cargo_options_build_args_with_env_does_not_leak_into_args() {
        // Sanity: env values are not added as command-line args.
        let opts = CargoOptions {
            env: vec![("A".into(), "1".into())],
            ..CargoOptions::default()
        };
        let args = opts.build_command_args();
        assert!(args.iter().all(|a| !a.contains("A=1") && !a.contains("=1")));
    }
}

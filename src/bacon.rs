use std::env;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::{Deserialize, Serialize};
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::LOCATIONS_FILE;

#[derive(Debug, Deserialize, Serialize)]
struct BaconConfig {
    jobs: Jobs,
    exports: Exports,
}

#[derive(Debug, Deserialize, Serialize)]
struct Jobs {
    #[serde(rename = "bacon-ls")]
    bacon_ls: BaconLs,
}

#[derive(Debug, Deserialize, Serialize)]
struct BaconLs {
    #[serde(skip_deserializing)]
    command: Vec<String>,
    analyzer: String,
    need_stdout: bool,
}

#[derive(Debug, Deserialize, Serialize)]
struct Exports {
    #[serde(rename = "cargo-json-spans")]
    cargo_json_spans: CargoJsonSpans,
}

#[derive(Debug, Deserialize, Serialize)]
struct CargoJsonSpans {
    auto: bool,
    exporter: String,
    line_format: String,
    path: String,
}

const ERROR_MESSAGE: &str = "bacon configuration is not compatible with bacon-ls: please take a look to https://github.com/crisidev/bacon-ls?tab=readme-ov-file#configuration and adapt your bacon configuration";
const BACON_ANALYZER: &str = "cargo_json";
const BACON_EXPORTER: &str = "analyzer";
const BACON_COMMAND: [&str; 7] = [
    "cargo",
    "clippy",
    "--tests",
    "--all-targets",
    "--all-features",
    "--message-format",
    "json-diagnostic-rendered-ansi",
];
const LINE_FORMAT: &str = "{diagnostic.level}|:|{span.file_name}|:|{span.line_start}|:|{span.line_end}|:|{span.column_start}|:|{span.column_end}|:|{diagnostic.message}|:|{diagnostic.rendered}|:|{span.suggested_replacement}";

pub(crate) struct Bacon;

impl Bacon {
    async fn validate_preferences_file(path: &Path) -> Result<(), String> {
        let toml_content = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| format!("{ERROR_MESSAGE}: {e}"))?;
        let config: BaconConfig = toml::from_str(&toml_content).map_err(|e| format!("{ERROR_MESSAGE}: {e}"))?;
        tracing::debug!("bacon config is {config:#?}");
        if config.jobs.bacon_ls.analyzer == BACON_ANALYZER
            && config.jobs.bacon_ls.need_stdout
            && config.exports.cargo_json_spans.auto
            && config.exports.cargo_json_spans.exporter == BACON_EXPORTER
            && config.exports.cargo_json_spans.line_format == LINE_FORMAT
            && config.exports.cargo_json_spans.path == LOCATIONS_FILE
        {
            tracing::info!("bacon configuration {} is valid", path.display());
            Ok(())
        } else {
            Err(ERROR_MESSAGE.to_string())
        }
    }

    async fn create_preferences_file(filename: &str) -> Result<(), String> {
        let bacon_config = BaconConfig {
            jobs: Jobs {
                bacon_ls: BaconLs {
                    command: BACON_COMMAND.map(|c| c.to_string()).into_iter().collect(),
                    analyzer: BACON_ANALYZER.to_string(),
                    need_stdout: true,
                },
            },
            exports: Exports {
                cargo_json_spans: CargoJsonSpans {
                    auto: true,
                    exporter: BACON_EXPORTER.to_string(),
                    line_format: LINE_FORMAT.to_string(),
                    path: LOCATIONS_FILE.to_string(),
                },
            },
        };
        tracing::info!("creating new bacon preference file {filename}",);
        let toml_string = toml::to_string_pretty(&bacon_config)
            .map_err(|e| format!("error serializing bacon preferences {filename} content: {e}"))?;
        let mut file = File::create(filename)
            .await
            .map_err(|e| format!("error creating bacon preferences {filename}: {e}"))?;
        file.write_all(toml_string.as_bytes())
            .await
            .map_err(|e| format!("error writing bacon preferences {filename}: {e}"))?;
        Ok(())
    }

    async fn validate_preferences_impl(bacon_prefs: &[u8], create_prefs_file: bool) -> Result<(), String> {
        let bacon_prefs_files = String::from_utf8_lossy(bacon_prefs);
        let bacon_prefs_files_split: Vec<&str> = bacon_prefs_files.split("\n").collect();
        let mut preference_file_exists = false;
        for prefs_file in bacon_prefs_files_split.iter() {
            let prefs_file_path = Path::new(prefs_file);
            if prefs_file_path.exists() {
                preference_file_exists = true;
                Self::validate_preferences_file(prefs_file_path).await?;
            } else {
                tracing::debug!("skipping non existing bacon preference file {prefs_file}");
            }
        }

        if !preference_file_exists && create_prefs_file {
            Self::create_preferences_file(bacon_prefs_files_split[0]).await?;
        }

        Ok(())
    }

    pub(crate) async fn validate_preferences(create_prefs_file: bool) -> Result<(), String> {
        let bacon_prefs = Command::new("bacon")
            .arg("--prefs")
            .output()
            .await
            .map_err(|e| e.to_string())?;
        Self::validate_preferences_impl(&bacon_prefs.stdout, create_prefs_file).await
    }

    pub(crate) async fn run_in_background(
        bacon_command: &str,
        bacon_command_args: &str,
        current_dir: Option<&PathBuf>,
        cancel_token: CancellationToken,
    ) -> Result<JoinHandle<()>, String> {
        tracing::info!("starting bacon in background with arguments `{bacon_command_args}`");
        let log_bacon = env::var("BACON_LS_LOG_BACON").unwrap_or("on".to_string());
        let mut command = Command::new(bacon_command);
        command
            .args(bacon_command_args.split_whitespace().collect::<Vec<&str>>())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(current_dir) = current_dir {
            command.current_dir(current_dir);
        }

        match command.spawn() {
            Ok(mut child) => {
                // Handle stdout
                if log_bacon != "off" {
                    if let Some(stdout) = child.stdout.take() {
                        let reader = BufReader::new(stdout).lines();
                        tokio::spawn(async move {
                            let mut reader = reader;
                            while let Ok(Some(line)) = reader.next_line().await {
                                tracing::info!("[bacon stdout]: {}", line);
                            }
                        });
                    }
                }

                // Handle stderr
                if log_bacon != "off" {
                    if let Some(stderr) = child.stderr.take() {
                        let reader = BufReader::new(stderr).lines();
                        tokio::spawn(async move {
                            let mut reader = reader;
                            while let Ok(Some(line)) = reader.next_line().await {
                                tracing::error!("[bacon stderr]: {}", line);
                            }
                        });
                    }
                }

                // Wait for the child process to finish
                Ok(tokio::spawn(async move {
                    tracing::debug!("waiting for bacon to terminate");
                    tokio::select! {
                        _ = child.wait() => {},
                        _ = cancel_token.cancelled() => {},
                    };
                }))
            }
            Err(e) => Err(format!("failed to start bacon: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_valid_bacon_preferences() {
        let valid_toml = format!(
            r#"
            [jobs.bacon-ls]
            analyzer = "{BACON_ANALYZER}"
            need_stdout = true

            [exports.cargo-json-spans]
            auto = true
            exporter = "{BACON_EXPORTER}"
            line_format = "{LINE_FORMAT}"
            path = "{LOCATIONS_FILE}"
        "#
        );
        let tmp_dir = TempDir::new().unwrap();
        let file_path = tmp_dir.path().join("prefs.toml");
        let mut file = std::fs::File::create(&file_path).unwrap();
        write!(file, "{}", valid_toml).unwrap();
        assert!(Bacon::validate_preferences_file(&file_path).await.is_ok());
    }

    #[tokio::test]
    async fn test_invalid_analyzer() {
        let invalid_toml = format!(
            r#"
            [jobs.bacon-ls]
            analyzer = "incorrect_analyzer"
            need_stdout = true

            [exports.cargo-json-spans]
            auto = true
            exporter = "{BACON_EXPORTER}"
            line_format = "{LINE_FORMAT}"
            path = "{LOCATIONS_FILE}"
        "#
        );

        let tmp_dir = TempDir::new().unwrap();
        let file_path = tmp_dir.path().join("prefs.toml");
        let mut file = std::fs::File::create(&file_path).unwrap();
        write!(file, "{}", invalid_toml).unwrap();
        assert!(Bacon::validate_preferences_file(&file_path).await.is_err());
    }

    #[tokio::test]
    async fn test_invalid_line_format() {
        let invalid_toml = format!(
            r#"
            [jobs.bacon-ls]
            analyzer = "{BACON_ANALYZER}"
            need_stdout = true

            [exports.cargo-json-spans]
            auto = true
            exporter = "{BACON_EXPORTER}"
            line_format = "invalid_line_format"
            path = "{LOCATIONS_FILE}"
        "#
        );

        let tmp_dir = TempDir::new().unwrap();
        let file_path = tmp_dir.path().join("prefs.toml");
        let mut file = std::fs::File::create(&file_path).unwrap();
        write!(file, "{}", invalid_toml).unwrap();
        assert!(Bacon::validate_preferences_file(&file_path).await.is_err());
    }

    #[tokio::test]
    async fn test_validate_preferences() {
        let valid_toml = format!(
            r#"
            [jobs.bacon-ls]
            analyzer = "{BACON_ANALYZER}"
            need_stdout = true

            [exports.cargo-json-spans]
            auto = true
            exporter = "{BACON_EXPORTER}"
            line_format = "{LINE_FORMAT}"
            path = "{LOCATIONS_FILE}"
        "#
        );
        assert!(
            Bacon::validate_preferences_impl(valid_toml.as_bytes(), false)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn test_file_creation_failure() {
        let invalid_path = "/invalid/path/to/file.toml";
        let result = Bacon::create_preferences_file(invalid_path).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("error creating bacon preferences"));
    }

    #[tokio::test]
    async fn test_file_write_failure() {
        let tmp_dir = TempDir::new().unwrap();
        let file_path = tmp_dir.path().join("prefs.toml");
        // Simulate write failure by closing the file prematurely
        let file = File::create(&file_path).await.unwrap();
        drop(file); // Close the file to simulate failure
        let result = Bacon::create_preferences_file(file_path.to_str().unwrap()).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_empty_bacon_preferences_file() {
        let tmp_dir = TempDir::new().unwrap();
        let file_path = tmp_dir.path().join("empty_prefs.toml");
        std::fs::File::create(&file_path).unwrap();
        assert!(Bacon::validate_preferences_file(&file_path).await.is_err());
    }

    #[tokio::test]
    async fn test_run_in_background() {
        let cancel_token = CancellationToken::new();
        let handle = Bacon::run_in_background("echo", "I am running", None, cancel_token.clone()).await;
        assert!(handle.is_ok());
        cancel_token.cancel();
        handle.unwrap().await.unwrap();
    }
}

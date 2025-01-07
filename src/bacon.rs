use std::path::Path;

use serde::Deserialize;
use tokio::process::Command;

use crate::LOCATIONS_FILE;

#[derive(Debug, Deserialize)]
struct BaconConfig {
    jobs: Jobs,
    exports: Exports,
}

#[derive(Debug, Deserialize)]
struct Jobs {
    #[serde(rename = "bacon-ls")]
    bacon_ls: BaconLs,
}

#[derive(Debug, Deserialize)]
struct BaconLs {
    analyzer: String,
    need_stdout: bool,
}

#[derive(Debug, Deserialize)]
struct Exports {
    #[serde(rename = "cargo-json-spans")]
    cargo_json_spans: CargoJsonSpans,
}

#[derive(Debug, Deserialize)]
struct CargoJsonSpans {
    auto: bool,
    exporter: String,
    line_format: String,
    path: String,
}

const ERROR_MESSAGE: &str = "bacon configuration is not compatible with bacon-ls: please take a look to https://github.com/crisidev/bacon-ls?tab=readme-ov-file#configuration and adapt your bacon configuration";
const BACON_ANALYZER: &str = "cargo_json";
const LINE_FORMAT: &str = "{diagnostic.level}|:|{span.file_name}|:|{span.line_start}|:|{span.line_end}|:|{span.column_start}|:|{span.column_end}|:|{diagnostic.message}|:|{span.suggested_replacement}";

async fn validate_bacon_preferences_file(path: &Path) -> Result<(), String> {
    let toml_content = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| format!("{ERROR_MESSAGE}: {e}"))?;
    let config: BaconConfig =
        toml::from_str(&toml_content).map_err(|e| format!("{ERROR_MESSAGE}: {e}"))?;
    tracing::debug!("bacon config is {config:#?}");
    if config.jobs.bacon_ls.analyzer == BACON_ANALYZER
        && config.jobs.bacon_ls.need_stdout
        && config.exports.cargo_json_spans.auto
        && config.exports.cargo_json_spans.exporter == "analyzer"
        && config.exports.cargo_json_spans.line_format == LINE_FORMAT
        && config.exports.cargo_json_spans.path == LOCATIONS_FILE {
        tracing::info!("bacon configuration {} is valid", path.display());
        Ok(())
    } else {
        Err(ERROR_MESSAGE.to_string())
    }
}

pub(crate) async fn validate_bacon_preferences() -> Result<(), String> {
    let bacon_prefs = Command::new("bacon")
        .arg("--prefs")
        .output()
        .await
        .map_err(|e| e.to_string())?;
    let bacon_prefs_files = String::from_utf8_lossy(&bacon_prefs.stdout);
    for prefs_file in bacon_prefs_files.split("\n") {
        let prefs_file_path = Path::new(prefs_file);
        tracing::debug!("skipping non existing bacon preference file {prefs_file}");
        if prefs_file_path.exists() {
            validate_bacon_preferences_file(prefs_file_path).await?;
        }
    }
    Ok(())
}

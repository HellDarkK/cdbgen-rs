use std::{io, path::PathBuf};

use thiserror::Error;

use crate::{EXIT_CONFIG, EXIT_FETCH, EXIT_GENERIC, EXIT_OUTPUT};

#[derive(Debug, Error)]
pub enum AppError {
    #[error("config error: {0}")]
    Config(#[from] ConfigError),

    #[error("fetch error: {0}")]
    Fetch(#[from] FetchError),

    #[error("output error: {0}")]
    Output(#[from] OutputError),

    #[error("parse task failed: {0}")]
    ParseTask(String),

    #[error("{0}")]
    Generic(String),
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read {path}: {source}")]
    Read { path: PathBuf, source: io::Error },

    #[error("invalid TOML in {path}: {source}")]
    Toml {
        path: PathBuf,
        source: toml::de::Error,
    },

    #[error("configuration must contain at least one source")]
    EmptySources,

    #[error("configuration must contain at least one output")]
    EmptyOutputs,

    #[error("invalid source id {0:?}; expected ^[a-zA-Z0-9_-]+$")]
    InvalidSourceId(String),

    #[error("invalid URL for source {source_id}: {url}")]
    InvalidUrl { source_id: String, url: String },

    #[error("unsupported URL scheme for source {source_id}: {scheme}")]
    UnsupportedScheme { source_id: String, scheme: String },

    #[error("output group {group:?} does not reference any sources")]
    EmptyOutputGroup { group: String },

    #[error("output group {group:?} references unknown source {source_id:?}")]
    UnknownOutputSource { group: String, source_id: String },

    #[error("output group {group:?} has empty output path")]
    EmptyOutputPath { group: String },
}

#[derive(Debug, Error, Clone)]
pub enum FetchError {
    #[error("source {source_id} failed after retries and no usable cache exists: {message}")]
    Unavailable { source_id: String, message: String },

    #[error("cache error for source {source_id}: {message}")]
    Cache { source_id: String, message: String },
}

#[derive(Debug, Error)]
pub enum OutputError {
    #[error("failed to create parent directory {path}: {source}")]
    CreateParent { path: PathBuf, source: io::Error },

    #[error("failed to create temporary output {path}: {source}")]
    CreateTemp { path: PathBuf, source: io::Error },

    #[error("failed to write CDB record for {path}: {source}")]
    WriteRecord { path: PathBuf, source: io::Error },

    #[error("failed to finalize CDB {path}: {source}")]
    Finalize { path: PathBuf, source: io::Error },

    #[error("failed to fsync {path}: {source}")]
    Fsync { path: PathBuf, source: io::Error },

    #[error("failed to atomically replace {path}: {source}")]
    Rename { path: PathBuf, source: io::Error },
}

pub fn exit_code_for_error(error: &AppError) -> i32 {
    match error {
        AppError::Config(_) => EXIT_CONFIG,
        AppError::Fetch(_) => EXIT_FETCH,
        AppError::Output(_) => EXIT_OUTPUT,
        AppError::ParseTask(_) | AppError::Generic(_) => EXIT_GENERIC,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_exit_codes() {
        assert_eq!(
            exit_code_for_error(&AppError::Generic("x".into())),
            EXIT_GENERIC
        );
        assert_eq!(
            exit_code_for_error(&AppError::Config(ConfigError::EmptySources)),
            EXIT_CONFIG
        );
        assert_eq!(
            exit_code_for_error(&AppError::Fetch(FetchError::Unavailable {
                source_id: "a".into(),
                message: "x".into()
            })),
            EXIT_FETCH
        );
    }
}

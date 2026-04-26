use std::path::PathBuf;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, RsnipError>;

#[derive(Debug, Error)]
pub enum RsnipError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("failed to parse config {path}: {source}")]
    ConfigParse {
        path: PathBuf,
        source: toml::de::Error,
    },

    #[error("failed to serialize config: {0}")]
    ConfigSerialize(#[from] toml::ser::Error),

    #[error("invalid config value `{field}`: {message}")]
    InvalidConfig {
        field: &'static str,
        message: String,
    },

    #[error("could not resolve a valid user directory for {0}")]
    MissingUserDirectory(&'static str),

    #[error("unknown command `{0}`")]
    UnknownCommand(String),

    #[error("{0}")]
    Message(String),
}

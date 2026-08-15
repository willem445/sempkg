use thiserror::Error;

#[derive(Debug, Error)]
pub enum SempkgError {
    #[error("Bundle not found: {name}@{version}")]
    BundleNotFound { name: String, version: String },

    #[error("Bundle already installed: {name}@{version}")]
    AlreadyInstalled { name: String, version: String },

    #[error("Checksum mismatch for {path}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        path: String,
        expected: String,
        actual: String,
    },

    #[error("Registry error for {url}: {message}")]
    RegistryError { url: String, message: String },

    #[error("No LanceDB documentation index found in bundle '{0}'")]
    NoLanceIndex(String),

    #[error("LanceDB error: {0}")]
    LanceError(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("TOML parse error: {0}")]
    TomlParse(#[from] toml::de::Error),
}

pub type Result<T> = std::result::Result<T, SempkgError>;

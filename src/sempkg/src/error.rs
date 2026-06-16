use thiserror::Error;

#[derive(Debug, Error)]
pub enum SempkgError {
    #[error("Manifest not found at {path}. Run 'sempkg init' to create one.")]
    ManifestNotFound { path: String },

    #[error("Bundle not found: {name}@{version}")]
    BundleNotFound { name: String, version: String },

    #[error("Package not found: {0}")]
    PackageNotFound(String),

    #[error("Bundle already installed: {name}@{version}")]
    AlreadyInstalled { name: String, version: String },

    #[error("Checksum mismatch for {path}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        path: String,
        expected: String,
        actual: String,
    },

    #[error("Invalid bundle archive: {0}")]
    InvalidBundle(String),

    #[error("Signature verification failed: {0}")]
    SignatureVerificationFailed(String),

    #[error("Registry error for {url}: {message}")]
    RegistryError { url: String, message: String },

    #[error("codegraph not found on PATH. Install it with: npm install -g @colbymchenry/codegraph")]
    CodegraphNotFound,

    #[error("codegraph error: {0}")]
    CodegraphError(String),

    #[error("QMD not found on PATH. Install it with: npm install -g @tobilu/qmd")]
    QmdNotFound,

    #[error("No QMD index found in bundle '{0}'")]
    NoQmdIndex(String),

    #[error("Package '{0}' is not indexed. Run 'sempkg reindex {0}' first.")]
    NotIndexed(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("TOML parse error: {0}")]
    TomlParse(#[from] toml::de::Error),

    #[error("Database error: {0}")]
    Db(#[from] rusqlite::Error),
}

pub type Result<T> = std::result::Result<T, SempkgError>;

use thiserror::Error;

/// Errors produced by the SemBundle packer.
///
/// Error codes align with the validation rules in sembundle-spec.md §10-11.
#[derive(Debug, Error)]
pub enum PackError {
    /// E_MISSING_DIR — a required subdirectory is absent from the input.
    #[error("required directory not found: '{0}' (expected inside the CodeGraph output dir)")]
    MissingDirectory(String),

    /// E_MISSING_FILE — a required file is absent from the input.
    #[error("required file not found: '{0}' (expected inside the CodeGraph output dir)")]
    MissingFile(String),

    /// E_EMPTY_DIR — a required directory exists but contains no files.
    #[error("directory '{0}' is empty — at least one file must be present")]
    EmptyDirectory(String),

    /// E_INVALID_FIELD — a CLI-supplied value fails format validation.
    #[error("invalid value for '{field}': {reason}")]
    InvalidField { field: String, reason: String },

    /// I/O failure (file creation, read, write, etc.).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON serialization failure.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// Directory traversal failure.
    #[error("directory walk error: {0}")]
    Walk(#[from] walkdir::Error),

    /// Native `semgraph` indexer failure while building the `graph/` store.
    #[error("graph indexing error: {0}")]
    Index(#[from] semgraph::Error),
}

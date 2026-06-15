use std::path::Path;

use walkdir::WalkDir;

use crate::error::PackError;

/// Subdirectories that must exist and be non-empty in the CodeGraph output dir.
const REQUIRED_DIRS: &[&str] = &["graph", "embeddings"];

/// Files that must exist in the CodeGraph output dir.
const REQUIRED_FILES: &[&str] = &["config.json"];

/// Validate that the CodeGraph output directory has the required structure.
///
/// Checks:
/// - `graph/`      — exists and contains at least one file
/// - `embeddings/` — exists and contains at least one file
/// - `config.json` — exists as a regular file
pub fn validate_input_dir(dir: &Path) -> Result<(), PackError> {
    for subdir in REQUIRED_DIRS {
        let path = dir.join(subdir);
        if !path.is_dir() {
            return Err(PackError::MissingDirectory(subdir.to_string()));
        }
        let has_files = WalkDir::new(&path)
            .min_depth(1)
            .into_iter()
            .filter_map(|e| e.ok())
            .any(|e| e.file_type().is_file());
        if !has_files {
            return Err(PackError::EmptyDirectory(subdir.to_string()));
        }
    }

    for file in REQUIRED_FILES {
        if !dir.join(file).is_file() {
            return Err(PackError::MissingFile(file.to_string()));
        }
    }

    Ok(())
}

/// Validate that an optional QMD index directory has the required structure.
///
/// When `qmd/` is included in a bundle the following must be present
/// (spec §9.1):
/// - `index/index.sqlite` — project-local QMD database
/// - `embeddings/`        — non-empty vector export directory
/// - `metadata.json`      — QMD indexing metadata
/// - `config.json`        — QMD collection configuration
///
/// `model.gguf` is optional and is not validated here.
pub fn validate_qmd_dir(dir: &Path) -> Result<(), PackError> {
    // Required files (relative to qmd_dir)
    let required_files = [
        "index/index.sqlite",
        "metadata.json",
        "config.json",
    ];
    for file in &required_files {
        let path = dir.join(file);
        if !path.is_file() {
            return Err(PackError::MissingFile(format!("qmd/{file}")));
        }
    }

    // `embeddings/` must exist and be non-empty
    let embeddings = dir.join("embeddings");
    if !embeddings.is_dir() {
        return Err(PackError::MissingDirectory("qmd/embeddings".to_string()));
    }
    let has_files = WalkDir::new(&embeddings)
        .min_depth(1)
        .into_iter()
        .filter_map(|e| e.ok())
        .any(|e| e.file_type().is_file());
    if !has_files {
        return Err(PackError::EmptyDirectory("qmd/embeddings".to_string()));
    }

    Ok(())
}

/// Validate a bundle package name.
///
/// Rules (spec §4.2):
/// - At least 2 characters
/// - Only lowercase ASCII letters, digits, and hyphens
/// - Must start and end with a lowercase letter or digit
pub fn validate_name(name: &str) -> Result<(), PackError> {
    let err = |reason: &str| PackError::InvalidField {
        field: "name".to_string(),
        reason: reason.to_string(),
    };

    if name.len() < 2 {
        return Err(err("must be at least 2 characters"));
    }

    let first = name.chars().next().unwrap();
    let last = name.chars().last().unwrap();

    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return Err(err("must start with a lowercase letter or digit"));
    }
    if !last.is_ascii_lowercase() && !last.is_ascii_digit() {
        return Err(err("must end with a lowercase letter or digit"));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(err(
            "may only contain lowercase letters, digits, and hyphens",
        ));
    }

    Ok(())
}

/// Validate a Git commit hash.
///
/// Must be exactly 40 lowercase hexadecimal characters (spec §4.2).
pub fn validate_commit_hash(hash: &str) -> Result<(), PackError> {
    if hash.len() != 40 || !hash.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')) {
        return Err(PackError::InvalidField {
            field: "commit_hash".to_string(),
            reason: "must be a 40-character lowercase hex string".to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- validate_name ---

    #[test]
    fn valid_names() {
        for name in &["aws-sdk", "qt", "ros2-humble", "my-lib-v2"] {
            assert!(validate_name(name).is_ok(), "expected valid: {name}");
        }
    }

    #[test]
    fn name_too_short() {
        assert!(validate_name("a").is_err());
    }

    #[test]
    fn name_starts_with_hyphen() {
        assert!(validate_name("-sdk").is_err());
    }

    #[test]
    fn name_ends_with_hyphen() {
        assert!(validate_name("sdk-").is_err());
    }

    #[test]
    fn name_uppercase_rejected() {
        assert!(validate_name("MySDK").is_err());
    }

    #[test]
    fn name_with_spaces_rejected() {
        assert!(validate_name("my sdk").is_err());
    }

    // --- validate_commit_hash ---

    #[test]
    fn valid_commit_hash() {
        assert!(validate_commit_hash(&"a".repeat(40)).is_ok());
        assert!(validate_commit_hash("d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3").is_ok());
    }

    #[test]
    fn short_hash_rejected() {
        assert!(validate_commit_hash("abc123").is_err());
    }

    #[test]
    fn uppercase_hash_rejected() {
        assert!(validate_commit_hash(&"A".repeat(40)).is_err());
    }

    #[test]
    fn hash_with_non_hex_rejected() {
        let bad: String = "g".repeat(40);
        assert!(validate_commit_hash(&bad).is_err());
    }

    // --- validate_qmd_dir ---

    use std::fs;
    use tempfile::TempDir;

    fn make_qmd_dir(dir: &Path) {
        fs::create_dir_all(dir.join("index")).unwrap();
        fs::create_dir_all(dir.join("embeddings")).unwrap();
        fs::write(dir.join("index").join("index.sqlite"), b"sqlite-data").unwrap();
        fs::write(dir.join("embeddings").join("vectors.bin"), b"vec-data").unwrap();
        fs::write(dir.join("metadata.json"), b"{}").unwrap();
        fs::write(dir.join("config.json"), b"{}").unwrap();
    }

    #[test]
    fn valid_qmd_dir() {
        let tmp = TempDir::new().unwrap();
        make_qmd_dir(tmp.path());
        assert!(validate_qmd_dir(tmp.path()).is_ok());
    }

    #[test]
    fn qmd_missing_sqlite() {
        let tmp = TempDir::new().unwrap();
        make_qmd_dir(tmp.path());
        fs::remove_file(tmp.path().join("index").join("index.sqlite")).unwrap();
        let err = validate_qmd_dir(tmp.path()).unwrap_err();
        assert!(
            matches!(err, PackError::MissingFile(ref f) if f.contains("index.sqlite")),
            "unexpected: {err}"
        );
    }

    #[test]
    fn qmd_missing_metadata_json() {
        let tmp = TempDir::new().unwrap();
        make_qmd_dir(tmp.path());
        fs::remove_file(tmp.path().join("metadata.json")).unwrap();
        let err = validate_qmd_dir(tmp.path()).unwrap_err();
        assert!(
            matches!(err, PackError::MissingFile(ref f) if f.contains("metadata.json")),
            "unexpected: {err}"
        );
    }

    #[test]
    fn qmd_missing_config_json() {
        let tmp = TempDir::new().unwrap();
        make_qmd_dir(tmp.path());
        fs::remove_file(tmp.path().join("config.json")).unwrap();
        let err = validate_qmd_dir(tmp.path()).unwrap_err();
        assert!(
            matches!(err, PackError::MissingFile(ref f) if f.contains("config.json")),
            "unexpected: {err}"
        );
    }

    #[test]
    fn qmd_missing_embeddings_dir() {
        let tmp = TempDir::new().unwrap();
        make_qmd_dir(tmp.path());
        fs::remove_dir_all(tmp.path().join("embeddings")).unwrap();
        let err = validate_qmd_dir(tmp.path()).unwrap_err();
        assert!(
            matches!(err, PackError::MissingDirectory(ref d) if d.contains("embeddings")),
            "unexpected: {err}"
        );
    }

    #[test]
    fn qmd_empty_embeddings_dir() {
        let tmp = TempDir::new().unwrap();
        make_qmd_dir(tmp.path());
        fs::remove_file(tmp.path().join("embeddings").join("vectors.bin")).unwrap();
        let err = validate_qmd_dir(tmp.path()).unwrap_err();
        assert!(
            matches!(err, PackError::EmptyDirectory(ref d) if d.contains("embeddings")),
            "unexpected: {err}"
        );
    }
}

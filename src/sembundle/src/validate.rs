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

/// Validate that an optional Lance index directory has the required structure.
///
/// When `lance/` is included in a bundle the following must be present
/// (spec §9.1):
/// - `metadata.json`     — index metadata
/// - at least one `*.lance/` subdirectory  — the LanceDB table directory
pub fn validate_lance_dir(dir: &Path) -> Result<(), PackError> {
    // metadata.json is required
    if !dir.join("metadata.json").is_file() {
        return Err(PackError::MissingFile("lance/metadata.json".to_string()));
    }

    // At least one *.lance table directory must exist
    let has_table = std::fs::read_dir(dir)
        .map_err(|e| PackError::Io(e))?
        .filter_map(|e| e.ok())
        .any(|e| e.file_name().to_string_lossy().ends_with(".lance") && e.path().is_dir());

    if !has_table {
        return Err(PackError::MissingDirectory("lance/*.lance".to_string()));
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

/// Validate that an optional source-code index directory has the required structure.
///
/// When `code/` is included in a bundle the following must be present:
/// - `metadata.json`     — index metadata
/// - at least one `*.lance/` subdirectory  — the LanceDB table directory
pub fn validate_code_dir(dir: &Path) -> Result<(), PackError> {
    if !dir.join("metadata.json").is_file() {
        return Err(PackError::MissingFile("code/metadata.json".to_string()));
    }

    let has_table = std::fs::read_dir(dir)
        .map_err(PackError::Io)?
        .filter_map(|e| e.ok())
        .any(|e| e.file_name().to_string_lossy().ends_with(".lance") && e.path().is_dir());

    if !has_table {
        return Err(PackError::MissingDirectory("code/*.lance".to_string()));
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

    // --- validate_lance_dir ---

    use std::fs;
    use tempfile::TempDir;

    fn make_lance_dir(dir: &Path) {
        let table_dir = dir.join("docs.lance");
        fs::create_dir_all(&table_dir).unwrap();
        fs::write(table_dir.join("0.lance"), b"arrow-data").unwrap();
        fs::write(dir.join("metadata.json"), b"{}").unwrap();
    }

    #[test]
    fn valid_lance_dir() {
        let tmp = TempDir::new().unwrap();
        make_lance_dir(tmp.path());
        assert!(validate_lance_dir(tmp.path()).is_ok());
    }

    #[test]
    fn lance_missing_metadata_json() {
        let tmp = TempDir::new().unwrap();
        make_lance_dir(tmp.path());
        fs::remove_file(tmp.path().join("metadata.json")).unwrap();
        let err = validate_lance_dir(tmp.path()).unwrap_err();
        assert!(
            matches!(err, PackError::MissingFile(ref f) if f.contains("metadata.json")),
            "unexpected: {err}"
        );
    }

    #[test]
    fn lance_missing_table_dir() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("metadata.json"), b"{}").unwrap();
        let err = validate_lance_dir(tmp.path()).unwrap_err();
        assert!(
            matches!(err, PackError::MissingDirectory(_)),
            "unexpected: {err}"
        );
    }
}

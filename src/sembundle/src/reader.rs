//! Archive reader helpers for `.sembundle` files.
//!
//! These are the read-side counterpart to [`crate::pack`]. `sempkg` delegates to
//! them instead of re-implementing manifest reading, checksum validation, and the
//! `<name>-<version>/` prefix-stripping convention — keeping one owner of the
//! archive layout contract.

use std::io::{Cursor, Read};
use std::path::Path;

use thiserror::Error;

use crate::checksum::sha256_bytes;
use crate::consts::MANIFEST_FILE;
use crate::manifest::Manifest;

/// Errors produced while reading a `.sembundle` archive.
#[derive(Debug, Error)]
pub enum ReadError {
    #[error("I/O error reading bundle archive: {0}")]
    Io(#[from] std::io::Error),

    #[error("failed to parse {file} in bundle: {source}")]
    Json {
        file: String,
        #[source]
        source: serde_json::Error,
    },

    #[error("{0} not found in bundle archive")]
    MissingManifest(&'static str),

    #[error("checksum mismatch for '{path}': expected {expected}, got {actual}")]
    ChecksumMismatch {
        path: String,
        expected: String,
        actual: String,
    },
}

/// Strip the top-level `<name>-<version>/` directory from an archive entry path
/// and return the forward-slash bundle-relative key (e.g. `"graph/nodes.bin"`).
///
/// Returns `None` for the top-level directory entry itself (nothing follows the
/// prefix). This is the single definition of the prefix-stripping convention
/// shared by manifest reading, checksum validation, and extraction.
pub fn bundle_relative_key(archive_path: &Path) -> Option<String> {
    let mut components = archive_path.components();
    components.next()?; // drop `<name>-<version>/`
    let rest: Vec<String> = components
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if rest.is_empty() {
        None
    } else {
        Some(rest.join("/"))
    }
}

macro_rules! open_archive {
    ($bytes:expr) => {
        tar::Archive::new(flate2::read::GzDecoder::new(Cursor::new($bytes)))
    };
}

/// Read every regular file in the archive into memory as
/// `(bundle-relative key, bytes)` pairs.
///
/// Convenient for small bundles and tests; for large archives prefer streaming
/// with [`bundle_relative_key`] so file contents are not all held at once.
pub fn read_entries(bytes: &[u8]) -> Result<Vec<(String, Vec<u8>)>, ReadError> {
    let mut archive = open_archive!(bytes);
    let mut out = Vec::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path()?.to_path_buf();
        let Some(key) = bundle_relative_key(&path) else {
            continue;
        };
        let mut buf = Vec::new();
        entry.read_to_end(&mut buf)?;
        out.push((key, buf));
    }
    Ok(out)
}

/// Read and parse `manifest.json` from a `.sembundle` archive without extracting
/// the rest of the bundle. Stops as soon as the manifest is found.
pub fn read_manifest(bytes: &[u8]) -> Result<Manifest, ReadError> {
    let mut archive = open_archive!(bytes);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_path_buf();
        if bundle_relative_key(&path).as_deref() == Some(MANIFEST_FILE) {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf)?;
            return serde_json::from_slice(&buf).map_err(|source| ReadError::Json {
                file: MANIFEST_FILE.to_string(),
                source,
            });
        }
    }
    Err(ReadError::MissingManifest(MANIFEST_FILE))
}

/// Verify that every file listed in `manifest.checksums` matches its SHA-256
/// digest in the archive. Files not present in `checksums` (and `manifest.json`
/// itself) are ignored, mirroring the pack-time contract.
pub fn verify_checksums(bytes: &[u8], manifest: &Manifest) -> Result<(), ReadError> {
    let mut archive = open_archive!(bytes);
    for entry in archive.entries()? {
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path()?.to_path_buf();
        let Some(key) = bundle_relative_key(&path) else {
            continue;
        };
        if key == MANIFEST_FILE {
            continue;
        }
        if let Some(expected) = manifest.checksums.get(&key) {
            let mut data = Vec::new();
            entry.read_to_end(&mut data)?;
            let actual = sha256_bytes(&data);
            if &actual != expected {
                return Err(ReadError::ChecksumMismatch {
                    path: key,
                    expected: expected.clone(),
                    actual,
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn relative_key_strips_prefix() {
        assert_eq!(
            bundle_relative_key(Path::new("my-sdk-1.0.0/manifest.json")).as_deref(),
            Some("manifest.json")
        );
        assert_eq!(
            bundle_relative_key(Path::new("my-sdk-1.0.0/graph/nodes.bin")).as_deref(),
            Some("graph/nodes.bin")
        );
    }

    #[test]
    fn relative_key_of_top_level_dir_is_none() {
        assert_eq!(bundle_relative_key(Path::new("my-sdk-1.0.0")), None);
    }
}

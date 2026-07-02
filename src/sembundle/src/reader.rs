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

    #[error("bundle archive contains more than one {0} entry")]
    DuplicateManifest(&'static str),

    #[error("checksum mismatch for '{path}': expected {expected}, got {actual}")]
    ChecksumMismatch {
        path: String,
        expected: String,
        actual: String,
    },

    #[error("file '{0}' is listed in manifest checksums but missing from the archive")]
    MissingFile(String),
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
        // Normalise any backslash separators to '/' so keys match manifest.json
        // (which always uses '/'). On Linux `Path::components` does not treat '\'
        // as a separator, so an entry like `graph\nodes.bin` would otherwise never
        // match its `graph/nodes.bin` checksum key and be silently skipped. This
        // preserves the pre-workspace normalisation behaviour.
        Some(rest.join("/").replace('\\', "/"))
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
/// the rest of the bundle.
///
/// Rejects an archive that contains more than one `manifest.json` entry: since
/// the manifest is excluded from its own checksums, a second (unvalidated) copy
/// could otherwise overwrite the first on extraction, so the manifest we parse
/// here would not be the one that lands on disk.
pub fn read_manifest(bytes: &[u8]) -> Result<Manifest, ReadError> {
    let mut archive = open_archive!(bytes);
    let mut found: Option<Vec<u8>> = None;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_path_buf();
        if bundle_relative_key(&path).as_deref() == Some(MANIFEST_FILE) {
            if found.is_some() {
                return Err(ReadError::DuplicateManifest(MANIFEST_FILE));
            }
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf)?;
            found = Some(buf);
        }
    }
    let buf = found.ok_or(ReadError::MissingManifest(MANIFEST_FILE))?;
    serde_json::from_slice(&buf).map_err(|source| ReadError::Json {
        file: MANIFEST_FILE.to_string(),
        source,
    })
}

/// Verify that every file listed in `manifest.checksums` is present in the
/// archive and matches its recorded SHA-256 digest.
///
/// Guarantees, for a bundle that validates:
/// - every archive file whose key is in `checksums` hashes to the recorded value
///   (mismatch ⇒ [`ReadError::ChecksumMismatch`]); and
/// - every key in `checksums` corresponds to a file actually in the archive
///   (a listed-but-absent file ⇒ [`ReadError::MissingFile`]), so a truncated or
///   stripped bundle is rejected here rather than failing later at query time.
///
/// Files present in the archive but *not* listed in `checksums` (and
/// `manifest.json`, which is excluded from its own checksums) are ignored,
/// mirroring the pack-time coverage contract.
pub fn verify_checksums(bytes: &[u8], manifest: &Manifest) -> Result<(), ReadError> {
    let mut archive = open_archive!(bytes);
    let mut unseen: std::collections::BTreeSet<&str> =
        manifest.checksums.keys().map(String::as_str).collect();
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
            unseen.remove(key.as_str());
        }
    }
    if let Some(missing) = unseen.into_iter().next() {
        return Err(ReadError::MissingFile(missing.to_string()));
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

    #[test]
    fn relative_key_normalises_backslashes() {
        // A mixed-separator entry (e.g. from an older Windows packer) must still
        // produce a forward-slash key so it matches manifest.json checksum keys.
        assert_eq!(
            bundle_relative_key(Path::new("my-sdk-1.0.0/graph\\nodes.bin")).as_deref(),
            Some("graph/nodes.bin")
        );
    }

    /// Build a gzip tar archive in memory from `(path, bytes)` entries.
    fn make_archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
        use flate2::write::GzEncoder;
        use flate2::Compression;

        let mut buf = Vec::new();
        {
            let gz = GzEncoder::new(&mut buf, Compression::default());
            let mut builder = tar::Builder::new(gz);
            for (path, data) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(tar::EntryType::Regular);
                header.set_size(data.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder.append_data(&mut header, path, *data).unwrap();
            }
            builder.into_inner().unwrap().finish().unwrap();
        }
        buf
    }

    #[test]
    fn read_manifest_rejects_duplicate_manifest() {
        // Two manifest.json entries: the second is unvalidated (excluded from its
        // own checksums) yet would overwrite the first on extraction, so we must
        // refuse the archive rather than parse an ambiguous manifest.
        let archive = make_archive(&[
            ("pkg-1.0.0/manifest.json", b"{}"),
            ("pkg-1.0.0/manifest.json", b"{}"),
        ]);
        let err = read_manifest(&archive).unwrap_err();
        assert!(
            matches!(err, ReadError::DuplicateManifest(_)),
            "expected DuplicateManifest, got: {err}"
        );
    }
}

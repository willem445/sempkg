/// Bundle store: manages installed .sembundle archives at workspace or global scope.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::SempkgError;

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// Returns `<workspace>/.sempkg/bundles/`
pub fn workspace_store_dir(workspace_dir: &Path) -> PathBuf {
    workspace_dir.join(".sempkg").join("bundles")
}

/// Returns `~/.sempkg/bundles/`
pub fn global_store_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".sempkg")
        .join("bundles")
}

// ---------------------------------------------------------------------------
// Bundle manifest JSON (inside the .sembundle archive)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BundleManifest {
    pub spec_version: String,
    pub name: String,
    pub version: String,
    pub source_repo: String,
    pub commit_hash: String,
    pub tag: Option<String>,
    pub created_at: String,
    pub codegraph_version: String,
    #[serde(default)]
    pub extensions: Vec<String>,
    /// Embedding model baked into the shipped `lance`/`code` tables, if the
    /// bundle carries pre-computed vectors. Drives query-time model resolution.
    #[serde(default)]
    pub embedding_model: Option<String>,
    /// Dimension of the baked embedding vectors, if present.
    #[serde(default)]
    pub embedding_dim: Option<u32>,
    pub checksums: BTreeMap<String, String>,
}

impl BundleManifest {
    pub fn has_lance(&self) -> bool {
        self.extensions.iter().any(|e| e == "lance")
    }

    pub fn has_code(&self) -> bool {
        self.extensions.iter().any(|e| e == "code")
    }

    /// The embedding model the bundle shipped with, if any.
    pub fn embedding_model(&self) -> Option<&str> {
        self.embedding_model.as_deref()
    }
}

// ---------------------------------------------------------------------------
// Bundle info (installed)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct BundleInfo {
    pub name: String,
    pub version: String,
    /// Absolute path to the extracted bundle directory
    pub bundle_dir: PathBuf,
    pub manifest: BundleManifest,
    pub archive_sha256: String,
    pub scope: BundleScope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BundleScope {
    Workspace,
    Global,
}

impl BundleInfo {
    pub fn has_lance(&self) -> bool {
        self.manifest.has_lance() && self.bundle_dir.join("lance").is_dir()
    }

    pub fn has_code(&self) -> bool {
        self.manifest.has_code() && self.bundle_dir.join("code").is_dir()
    }

    pub fn is_indexed(&self) -> bool {
        // .codegraph/ must exist (created by create_codegraph_view after install),
        // and graph/ must be non-empty (the actual data from the bundle).
        self.bundle_dir.join(".codegraph").exists()
            && self.bundle_dir.join("graph").exists()
            && self
                .bundle_dir
                .join("graph")
                .read_dir()
                .map(|mut d| d.next().is_some())
                .unwrap_or(false)
    }
}

// ---------------------------------------------------------------------------
// BundleStore
// ---------------------------------------------------------------------------

pub struct BundleStore {
    store_dir: PathBuf,
    scope: BundleScope,
}

impl BundleStore {
    pub fn new(store_dir: PathBuf, scope: BundleScope) -> Self {
        Self { store_dir, scope }
    }

    pub fn workspace(workspace_dir: &Path) -> Self {
        Self::new(workspace_store_dir(workspace_dir), BundleScope::Workspace)
    }

    pub fn global() -> Self {
        Self::new(global_store_dir(), BundleScope::Global)
    }

    /// Directory for a specific bundle: `<store>/<name>/<version>/`
    pub fn bundle_dir(&self, name: &str, version: &str) -> PathBuf {
        self.store_dir.join(name).join(version)
    }

    pub fn is_installed(&self, name: &str, version: &str) -> bool {
        self.bundle_dir(name, version).exists()
    }

    /// Install a .sembundle file from disk into the store.
    pub fn install(&self, bundle_path: &Path) -> Result<BundleInfo> {
        let bytes = std::fs::read(bundle_path)
            .with_context(|| format!("Cannot read bundle: {}", bundle_path.display()))?;
        self.install_bytes(&bytes)
    }

    /// Install from raw bytes (already downloaded).
    pub fn install_bytes(&self, bytes: &[u8]) -> Result<BundleInfo> {
        use std::io::Cursor;

        // Read manifest.json first (need name/version to determine destination)
        let manifest = read_manifest_from_tar(bytes)?;

        let dest = self.bundle_dir(&manifest.name, &manifest.version);
        if dest.exists() {
            return Err(SempkgError::AlreadyInstalled {
                name: manifest.name.clone(),
                version: manifest.version.clone(),
            }
            .into());
        }

        let archive_sha256 = hex::encode(Sha256::digest(bytes));

        // Validate checksums before extracting
        validate_checksums(bytes, &manifest)?;

        // Extract into a temp dir first, then rename atomically
        let parent = dest.parent().unwrap();
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Cannot create store directory: {}", parent.display()))?;

        let tmp_dir =
            tempfile::tempdir_in(parent).context("Cannot create temp directory for extraction")?;

        let cursor = Cursor::new(bytes);
        let gz = flate2::read::GzDecoder::new(cursor);
        let mut archive = tar::Archive::new(gz);

        // Extract stripping the top-level `<name>-<version>/` prefix
        for entry in archive
            .entries()
            .context("Failed to read archive entries")?
        {
            let mut entry = entry.context("Bad archive entry")?;
            let entry_path = entry.path().context("Bad entry path")?;
            let entry_path = entry_path.to_path_buf();

            // Strip leading `<name>-<version>/`
            let stripped = entry_path.components().skip(1).collect::<PathBuf>();
            if stripped == PathBuf::from("") {
                continue; // top-level dir entry itself
            }

            let out_path = tmp_dir.path().join(&stripped);
            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if entry.header().entry_type().is_file() {
                entry
                    .unpack(&out_path)
                    .with_context(|| format!("Failed to extract {}", stripped.display()))?;
            }
        }

        std::fs::write(
            tmp_dir.path().join(INSTALL_METADATA_FILE),
            serde_json::to_vec_pretty(&InstallMetadata {
                archive_sha256: archive_sha256.clone(),
            })?,
        )
        .with_context(|| {
            format!(
                "Cannot write install metadata in {}",
                tmp_dir.path().display()
            )
        })?;

        // Move temp dir to final destination
        let tmp_path = tmp_dir.keep();
        std::fs::rename(&tmp_path, &dest)
            .with_context(|| format!("Cannot move bundle to {}", dest.display()))?;

        // Create .codegraph -> graph link so the codegraph CLI can find the index.
        // The sembundle spec stores the CodeGraph index in graph/ but the codegraph
        // CLI looks for .codegraph/ in the working directory.
        create_codegraph_view(&dest)?;

        Ok(BundleInfo {
            name: manifest.name.clone(),
            version: manifest.version.clone(),
            bundle_dir: dest,
            manifest,
            archive_sha256,
            scope: self.scope,
        })
    }

    /// List all installed bundles in this store.
    pub fn list(&self) -> Vec<BundleInfo> {
        if !self.store_dir.exists() {
            return Vec::new();
        }
        let mut result = Vec::new();
        if let Ok(names) = std::fs::read_dir(&self.store_dir) {
            for name_entry in names.flatten() {
                let name = name_entry.file_name().to_string_lossy().to_string();
                if let Ok(versions) = std::fs::read_dir(name_entry.path()) {
                    for ver_entry in versions.flatten() {
                        let version = ver_entry.file_name().to_string_lossy().to_string();
                        let bundle_dir = ver_entry.path();
                        let manifest_path = bundle_dir.join("manifest.json");
                        if let Ok(text) = std::fs::read_to_string(&manifest_path) {
                            if let Ok(manifest) = serde_json::from_str::<BundleManifest>(&text) {
                                let archive_sha256 = read_install_metadata(&bundle_dir)
                                    .map(|meta| meta.archive_sha256)
                                    .unwrap_or_default();
                                result.push(BundleInfo {
                                    name: name.clone(),
                                    version: version.clone(),
                                    bundle_dir,
                                    manifest,
                                    archive_sha256,
                                    scope: self.scope,
                                });
                            }
                        }
                    }
                }
            }
        }
        result
    }

    /// Get a specific installed bundle by name (latest version if multiple).
    pub fn get(&self, name: &str) -> Option<BundleInfo> {
        self.list().into_iter().filter(|b| b.name == name).last()
    }

    /// Get a specific version of a bundle.
    pub fn get_version(&self, name: &str, version: &str) -> Option<BundleInfo> {
        let bundle_dir = self.bundle_dir(name, version);
        let manifest_path = bundle_dir.join("manifest.json");
        if !manifest_path.exists() {
            return None;
        }
        let text = std::fs::read_to_string(&manifest_path).ok()?;
        let manifest = serde_json::from_str::<BundleManifest>(&text).ok()?;
        let archive_sha256 = read_install_metadata(&bundle_dir)
            .map(|meta| meta.archive_sha256)
            .unwrap_or_default();
        Some(BundleInfo {
            name: name.to_string(),
            version: version.to_string(),
            bundle_dir,
            manifest,
            archive_sha256,
            scope: self.scope,
        })
    }

    /// Remove an installed bundle from the store.
    pub fn remove(&self, name: &str, version: &str) -> Result<()> {
        let dir = self.bundle_dir(name, version);
        if !dir.exists() {
            return Err(SempkgError::BundleNotFound {
                name: name.to_string(),
                version: version.to_string(),
            }
            .into());
        }
        std::fs::remove_dir_all(&dir)
            .with_context(|| format!("Failed to remove bundle at {}", dir.display()))
    }

    /// Remove every installed version of a package from the store.
    pub fn remove_package(&self, name: &str) -> Result<usize> {
        let package_dir = self.store_dir.join(name);
        if !package_dir.exists() {
            return Ok(0);
        }

        let version_count = std::fs::read_dir(&package_dir)
            .with_context(|| format!("Failed to read package store at {}", package_dir.display()))?
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false))
            .count();

        std::fs::remove_dir_all(&package_dir).with_context(|| {
            format!(
                "Failed to remove package store at {}",
                package_dir.display()
            )
        })?;

        Ok(version_count)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Create a `.codegraph` directory view inside `bundle_dir` that points to
/// the `graph/` subdirectory, so the codegraph CLI can find the index.
///
/// On Windows: creates a directory junction (no elevation required).
/// On Unix:    creates a relative symlink.
///
/// Idempotent — silently skips if `.codegraph` already exists or `graph/` is absent.
pub fn create_codegraph_view(bundle_dir: &Path) -> Result<()> {
    let graph_dir = bundle_dir.join("graph");
    let link = bundle_dir.join(".codegraph");

    if !graph_dir.exists() || link.exists() {
        return Ok(());
    }

    #[cfg(windows)]
    {
        // Directory junctions do not require elevated privileges or developer mode.
        // We shell out to `cmd /c mklink /J` which is universally available.
        let status = std::process::Command::new("cmd")
            .args([
                "/C",
                "mklink",
                "/J",
                &link.to_string_lossy(),
                &graph_dir.to_string_lossy(),
            ])
            .output()
            .context("Failed to run mklink to create .codegraph junction")?;
        if !status.status.success() {
            let stderr = String::from_utf8_lossy(&status.stderr);
            let stdout = String::from_utf8_lossy(&status.stdout);
            anyhow::bail!(
                "mklink /J failed for bundle at {}: {}",
                bundle_dir.display(),
                if !stderr.is_empty() { stderr } else { stdout }
            );
        }
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("graph", &link).with_context(|| {
            format!(
                "Failed to create .codegraph symlink in {}",
                bundle_dir.display()
            )
        })?;
    }

    Ok(())
}

/// Repair the `.codegraph` view for an already-installed bundle that is missing it.
/// Call this if a bundle was installed before this fix was applied.
pub fn repair_codegraph_view(bundle_dir: &Path) -> Result<bool> {
    let link = bundle_dir.join(".codegraph");
    if link.exists() {
        return Ok(false); // already present
    }
    create_codegraph_view(bundle_dir)?;
    Ok(true)
}

const INSTALL_METADATA_FILE: &str = ".sempkg-install.json";

#[derive(Debug, Clone, Deserialize, Serialize)]
struct InstallMetadata {
    archive_sha256: String,
}

fn read_install_metadata(bundle_dir: &Path) -> Option<InstallMetadata> {
    let path = bundle_dir.join(INSTALL_METADATA_FILE);
    let text = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Read `manifest.json` from a .sembundle archive (without extracting).
pub fn read_manifest_from_tar(bytes: &[u8]) -> Result<BundleManifest> {
    use std::io::{Cursor, Read};

    let cursor = Cursor::new(bytes);
    let gz = flate2::read::GzDecoder::new(cursor);
    let mut archive = tar::Archive::new(gz);

    for entry in archive.entries().context("Failed to read archive")? {
        let mut entry = entry.context("Bad archive entry")?;
        let path = entry.path().context("Bad entry path")?.to_path_buf();
        let parts: Vec<_> = path.components().collect();

        // Looking for `<name>-<version>/manifest.json`
        if parts.len() == 2 && parts[1].as_os_str() == "manifest.json" {
            let mut buf = String::new();
            entry
                .read_to_string(&mut buf)
                .context("Failed to read manifest.json")?;
            return serde_json::from_str(&buf).context("Failed to parse manifest.json");
        }
    }
    anyhow::bail!("manifest.json not found in archive")
}

/// Validate all checksums listed in bundle manifest.json.
fn validate_checksums(bytes: &[u8], manifest: &BundleManifest) -> Result<()> {
    use std::io::{Cursor, Read};

    let cursor = Cursor::new(bytes);
    let gz = flate2::read::GzDecoder::new(cursor);
    let mut archive = tar::Archive::new(gz);

    for entry in archive
        .entries()
        .context("Failed to read archive for checksum validation")?
    {
        let mut entry = entry.context("Bad archive entry")?;
        let path = entry.path().context("Bad entry path")?.to_path_buf();
        let parts: Vec<_> = path.components().collect();
        if parts.len() < 2 {
            continue;
        }

        // Relative path within bundle (strip top-level dir)
        let rel: PathBuf = parts[1..].iter().collect();
        let rel_str = rel.to_string_lossy().replace('\\', "/");

        if rel_str == "manifest.json" {
            continue; // manifest itself is not checksummed
        }

        if let Some(expected) = manifest.checksums.get(&rel_str) {
            if !entry.header().entry_type().is_file() {
                continue;
            }
            let mut data = Vec::new();
            entry
                .read_to_end(&mut data)
                .context("Failed to read file for checksum")?;
            let actual = hex::encode(Sha256::digest(&data));
            if &actual != expected {
                return Err(SempkgError::ChecksumMismatch {
                    path: rel_str,
                    expected: expected.clone(),
                    actual,
                }
                .into());
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Multi-scope helpers
// ---------------------------------------------------------------------------

/// List bundles from both workspace and global stores.
pub fn list_all_bundles(workspace_dir: Option<&Path>) -> Vec<BundleInfo> {
    let mut result = Vec::new();
    if let Some(dir) = workspace_dir {
        result.extend(BundleStore::workspace(dir).list());
    }
    result.extend(BundleStore::global().list());
    result
}

/// Resolve a bundle by name from workspace (preferred) or global store.
pub fn resolve_bundle(name: &str, workspace_dir: Option<&Path>) -> Option<BundleInfo> {
    if let Some(dir) = workspace_dir {
        if let Some(b) = BundleStore::workspace(dir).get(name) {
            return Some(b);
        }
    }
    BundleStore::global().get(name)
}

/// Resolve a bundle from a spec that may be either `name` or `name@version`.
///
/// Query results identify packages as `name@version`, so any path that takes a
/// package identifier from a query hit (e.g. small-to-big expansion, the
/// `read_code` affordance) must accept the versioned form.  When a version is
/// present it is matched exactly, falling back to the latest installed version
/// of `name` if that exact version is not installed.
pub fn resolve_bundle_spec(spec: &str, workspace_dir: Option<&Path>) -> Option<BundleInfo> {
    if let Some((name, version)) = spec.rsplit_once('@') {
        if !name.is_empty() && !version.is_empty() {
            if let Some(dir) = workspace_dir {
                if let Some(b) = BundleStore::workspace(dir).get_version(name, version) {
                    return Some(b);
                }
            }
            if let Some(b) = BundleStore::global().get_version(name, version) {
                return Some(b);
            }
            // Exact version not installed — fall back to name-only resolution.
            return resolve_bundle(name, workspace_dir);
        }
    }
    resolve_bundle(spec, workspace_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_manifest_json(extra: &str) -> String {
        format!(
            r#"{{
                "spec_version": "1.4.0",
                "name": "demo",
                "version": "1.0.0",
                "source_repo": "https://example.com/demo",
                "commit_hash": "{hash}",
                "tag": null,
                "created_at": "1970-01-01T00:00:00Z",
                "codegraph_version": "0.3.1",
                "extensions": ["lance", "code"],
                {extra}
                "checksums": {{}}
            }}"#,
            hash = "a".repeat(40),
            extra = extra
        )
    }

    #[test]
    fn manifest_reads_embedding_fields() {
        let json = base_manifest_json(
            r#""embedding_model": "embeddinggemma-300m", "embedding_dim": 768,"#,
        );
        let m: BundleManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(m.embedding_model(), Some("embeddinggemma-300m"));
        assert_eq!(m.embedding_dim, Some(768));
    }

    #[test]
    fn manifest_without_embedding_fields_is_backward_compatible() {
        // Older bundles (pre-1.4.0) omit the fields entirely.
        let json = base_manifest_json("");
        let m: BundleManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(m.embedding_model(), None);
        assert_eq!(m.embedding_dim, None);
        assert!(m.has_lance() && m.has_code());
    }
}

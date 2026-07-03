/// Bundle store: manages installed .sembundle archives at workspace or global scope.
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use sembundle::consts::{CODE_DIR, CODE_EXT, GRAPH_DIR, LANCE_DIR, LANCE_EXT, MANIFEST_FILE};

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

/// The manifest schema is owned by `sembundle`; re-export it under the historical
/// `BundleManifest` name rather than keeping a second, drift-prone copy. It is
/// field-identical (both use a `BTreeMap` for `checksums`).
pub use sembundle::manifest::Manifest as BundleManifest;

/// Extension methods for querying which optional extensions a manifest declares.
///
/// These stay on the `sempkg` side (as an extension trait) rather than on the
/// shared type because "which extensions are present" is an install-store concern,
/// not part of the on-disk archive schema.
pub trait ManifestExt {
    fn has_lance(&self) -> bool;
    fn has_code(&self) -> bool;
}

impl ManifestExt for BundleManifest {
    fn has_lance(&self) -> bool {
        self.extensions.iter().any(|e| e == LANCE_EXT)
    }

    fn has_code(&self) -> bool {
        self.extensions.iter().any(|e| e == CODE_EXT)
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
        self.manifest.has_lance() && self.bundle_dir.join(LANCE_DIR).is_dir()
    }

    pub fn has_code(&self) -> bool {
        self.manifest.has_code() && self.bundle_dir.join(CODE_DIR).is_dir()
    }

    pub fn is_indexed(&self) -> bool {
        // .codegraph/ must exist (created by create_codegraph_view after install),
        // and graph/ must be non-empty (the actual data from the bundle).
        self.bundle_dir.join(".codegraph").exists()
            && self.bundle_dir.join(GRAPH_DIR).exists()
            && self
                .bundle_dir
                .join(GRAPH_DIR)
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
    #[allow(dead_code)] // file-path install kept as public API; the add path uses install_bytes
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

        let archive_sha256 = sembundle::checksum::sha256_bytes(bytes);

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
            let entry_path = entry.path().context("Bad entry path")?.to_path_buf();

            // Strip the leading `<name>-<version>/` using the shared convention.
            let Some(rel_key) = sembundle::reader::bundle_relative_key(&entry_path) else {
                continue; // top-level dir entry itself
            };

            // Guard against path traversal: `bundle_relative_key` preserves `..`
            // and (on Windows) drive prefixes, and `entry.unpack` performs no
            // containment check, so a crafted entry could escape the store even
            // with a matching checksum. Reject anything that isn't a plain,
            // store-relative path.
            if Path::new(&rel_key).components().any(|c| {
                matches!(
                    c,
                    std::path::Component::ParentDir
                        | std::path::Component::RootDir
                        | std::path::Component::Prefix(_)
                )
            }) {
                anyhow::bail!("Refusing to extract unsafe bundle entry path: {rel_key}");
            }

            let out_path = tmp_dir.path().join(&rel_key);
            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if entry.header().entry_type().is_file() {
                entry
                    .unpack(&out_path)
                    .with_context(|| format!("Failed to extract {rel_key}"))?;
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
                        let manifest_path = bundle_dir.join(MANIFEST_FILE);
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
        self.list().into_iter().rfind(|b| b.name == name)
    }

    /// Get a specific version of a bundle.
    pub fn get_version(&self, name: &str, version: &str) -> Option<BundleInfo> {
        let bundle_dir = self.bundle_dir(name, version);
        let manifest_path = bundle_dir.join(MANIFEST_FILE);
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
    let graph_dir = bundle_dir.join(GRAPH_DIR);
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
///
/// Delegates to the shared reader so the archive-layout convention lives in one
/// place ([`sembundle::reader`]).
pub fn read_manifest_from_tar(bytes: &[u8]) -> Result<BundleManifest> {
    Ok(sembundle::reader::read_manifest(bytes)?)
}

/// Validate all checksums listed in bundle manifest.json.
///
/// Delegates to [`sembundle::reader::verify_checksums`] — the same routine used
/// by the sembundle CLI — so writer and reader can't disagree on what's covered.
fn validate_checksums(bytes: &[u8], manifest: &BundleManifest) -> Result<()> {
    Ok(sembundle::reader::verify_checksums(bytes, manifest)?)
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

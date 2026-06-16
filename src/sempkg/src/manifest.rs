/// Workspace manifest: sempkg.toml and sempkg.lock
///
/// sempkg.toml format:
/// ```toml
/// [workspace]
/// verify_key = "path/to/pubkey.pem"   # optional Ed25519 public key
///
/// [[registry]]
/// name = "default"
/// url  = "https://registry.example.com"
///
/// [dependencies]
/// aws-sdk = { version = "1.11.210" }
/// qt      = { version = "6.7.0", registry = "other" }
///
/// [packages]
/// mylib = { path = "/home/user/repos/mylib", description = "My library" }
/// ```
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const MANIFEST_FILE: &str = "sempkg.toml";
pub const LOCK_FILE: &str = "sempkg.lock";

// ---------------------------------------------------------------------------
// Manifest types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WorkspaceSection {
    /// Path to an Ed25519 PEM public key for bundle signature verification.
    pub verify_key: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RegistryEntry {
    pub name: String,
    pub url: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DependencyEntry {
    pub version: String,
    /// Name of the registry to fetch from. Defaults to the first registry.
    pub registry: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PackageEntry {
    /// Absolute or `~`-prefixed path to the locally cloned repository.
    pub path: String,
    /// Optional one-line description.
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WorkspaceManifest {
    pub workspace: Option<WorkspaceSection>,
    #[serde(rename = "registry", default)]
    pub registries: Vec<RegistryEntry>,
    #[serde(default)]
    pub dependencies: BTreeMap<String, DependencyEntry>,
    #[serde(default)]
    pub packages: BTreeMap<String, PackageEntry>,
}

impl WorkspaceManifest {
    pub fn default_registry(&self) -> Option<&RegistryEntry> {
        self.registries.first()
    }

    pub fn get_registry(&self, name: &str) -> Option<&RegistryEntry> {
        self.registries.iter().find(|r| r.name == name)
    }

    /// Resolve the registry for a dependency (fallback to first registry).
    pub fn registry_for(&self, dep: &DependencyEntry) -> Option<&RegistryEntry> {
        if let Some(name) = &dep.registry {
            self.get_registry(name)
        } else {
            self.default_registry()
        }
    }

    /// Path to the verify key, resolved relative to `manifest_dir`.
    pub fn verify_key_path(&self, manifest_dir: &Path) -> Option<PathBuf> {
        self.workspace
            .as_ref()
            .and_then(|w| w.verify_key.as_deref())
            .map(|k| manifest_dir.join(k))
    }
}

// ---------------------------------------------------------------------------
// Lock file types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct LockFile {
    #[serde(rename = "package", default)]
    pub packages: Vec<LockEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LockEntry {
    pub name: String,
    pub version: String,
    pub registry_url: String,
    pub sha256: String,
    #[serde(default)]
    pub signed: bool,
    #[serde(default)]
    pub manifest_checksums: BTreeMap<String, String>,
}

impl LockFile {
    pub fn find(&self, name: &str) -> Option<&LockEntry> {
        self.packages.iter().find(|e| e.name == name)
    }

    pub fn upsert(&mut self, entry: LockEntry) {
        if let Some(existing) = self.packages.iter_mut().find(|e| e.name == entry.name) {
            *existing = entry;
        } else {
            self.packages.push(entry);
        }
    }
}

// ---------------------------------------------------------------------------
// I/O
// ---------------------------------------------------------------------------

pub fn load_manifest(workspace_dir: &Path) -> Result<WorkspaceManifest> {
    let path = workspace_dir.join(MANIFEST_FILE);
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("{MANIFEST_FILE} not found in {}. Run 'sempkg init' to create one.", workspace_dir.display()))?;
    toml::from_str(&text).with_context(|| format!("Failed to parse {MANIFEST_FILE}"))
}

pub fn save_manifest(manifest: &WorkspaceManifest, workspace_dir: &Path) -> Result<()> {
    let path = workspace_dir.join(MANIFEST_FILE);
    let header = "# sempkg workspace manifest\n# Run 'sempkg sync' to install dependencies.\n\n";
    let body = toml::to_string_pretty(manifest)
        .context("Failed to serialize manifest")?;
    std::fs::write(&path, format!("{header}{body}"))
        .with_context(|| format!("Failed to write {}", path.display()))
}

pub fn load_lock(workspace_dir: &Path) -> Result<LockFile> {
    let path = workspace_dir.join(LOCK_FILE);
    if !path.exists() {
        return Ok(LockFile::default());
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read {LOCK_FILE}"))?;
    toml::from_str(&text).with_context(|| format!("Failed to parse {LOCK_FILE}"))
}

pub fn save_lock(lock: &LockFile, workspace_dir: &Path) -> Result<()> {
    let path = workspace_dir.join(LOCK_FILE);
    let header = "# Auto-generated by sempkg. DO NOT EDIT.\n# Commit this file to ensure reproducible installs.\n\n";
    let body = toml::to_string_pretty(lock)
        .context("Failed to serialize lock file")?;
    std::fs::write(&path, format!("{header}{body}"))
        .with_context(|| format!("Failed to write {}", path.display()))
}

pub fn init_manifest(workspace_dir: &Path, registry_url: Option<&str>) -> Result<()> {
    let manifest_path = workspace_dir.join(MANIFEST_FILE);
    if manifest_path.exists() {
        anyhow::bail!("{MANIFEST_FILE} already exists at {}", workspace_dir.display());
    }

    let mut manifest = WorkspaceManifest {
        workspace: None,
        registries: Vec::new(),
        dependencies: BTreeMap::new(),
        packages: BTreeMap::new(),
    };

    if let Some(url) = registry_url {
        manifest.registries.push(RegistryEntry {
            name: "default".to_string(),
            url: url.trim_end_matches('/').to_string(),
        });
    }

    save_manifest(&manifest, workspace_dir)
}

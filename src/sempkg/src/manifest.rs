/// Workspace manifest: sempkg.toml and sempkg.lock
///
/// sempkg.toml format:
/// ```toml
/// # sempkg workspace manifest
///
/// [workspace]
/// verify_key = "path/to/pubkey.pem"   # optional Ed25519 public key
///
/// [[registry]]
/// name = "default"
/// url  = "https://registry.example.com"
///
/// [dependencies]
/// python-can = { version = "4.6.1" }
/// pyxcp      = { version = "0.21.5", registry = "other" }
///
/// # Optional named groups (installed with: sempkg sync --group dev)
/// [dependency-groups]
/// dev  = { pytest = { version = "7.4.0" } }
/// ml   = { torch = { version = "2.1.0" }, numpy = { version = "1.26.0" } }
///
/// [packages]
/// mylib = { path = "/home/user/repos/mylib", description = "My library" }
/// ```
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use toml_edit::{DocumentMut, InlineTable, Item, Table, Value, value};

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
    /// Direct download URL for the bundle asset (e.g. a GitHub release URL).
    /// When set, `registry` is ignored and the bundle is fetched from this URL.
    pub url: Option<String>,
    /// GitHub source shorthand, e.g. `"github:pandas-dev/pandas"`.
    /// Set when this dependency was added via `sempkg add <github-url>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<String>,
    /// The git ref originally requested (tag / branch / SHA).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    /// Optional repo-relative subdirectory (monorepo scoping).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subdir: Option<String>,
    /// When true, `sempkg sync` will perform a full `git clone` instead of
    /// downloading the GitHub-generated tar.gz archive.  Use for repos that
    /// strip documentation from their archive via `.gitattributes export-ignore`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub full: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PackageEntry {
    /// Absolute or `~`-prefixed path to the locally cloned repository.
    pub path: String,
    /// Optional one-line description.
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct WorkspaceManifest {
    pub workspace: Option<WorkspaceSection>,
    #[serde(rename = "registry", default)]
    pub registries: Vec<RegistryEntry>,
    /// Default dependencies — always installed by `sempkg sync`.
    #[serde(default)]
    pub dependencies: BTreeMap<String, DependencyEntry>,
    /// Named dependency groups — installed with `sempkg sync --group <name>`.
    /// Each group is a map of package-name → DependencyEntry.
    #[serde(rename = "dependency-groups", default)]
    pub dependency_groups: BTreeMap<String, BTreeMap<String, DependencyEntry>>,
    #[serde(default)]
    pub packages: BTreeMap<String, PackageEntry>,
    /// Optional local LLM reranker configuration.
    pub reranker: Option<crate::reranker::RerankerConfig>,
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

    /// Collect dependencies to install for the given group selection.
    ///
    /// Always includes `[dependencies]`. When `groups` is non-empty, those
    /// named groups from `[dependency-groups]` are merged in. When
    /// `all_groups` is true, every group is merged in.
    pub fn resolve_deps(
        &self,
        groups: &[String],
        all_groups: bool,
    ) -> BTreeMap<String, DependencyEntry> {
        let mut result = self.dependencies.clone();
        let selected: Vec<&str> = if all_groups {
            self.dependency_groups.keys().map(|s| s.as_str()).collect()
        } else {
            groups.iter().map(|s| s.as_str()).collect()
        };
        for group_name in &selected {
            if let Some(group) = self.dependency_groups.get(*group_name) {
                for (k, v) in group {
                    result.entry(k.clone()).or_insert_with(|| v.clone());
                }
            } else {
                eprintln!("Warning: dependency group '{group_name}' not found in sempkg.toml");
            }
        }
        result
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
    /// Resolved commit SHA for GitHub-sourced dependencies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_sha: Option<String>,
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
    let text = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "{MANIFEST_FILE} not found in {}. Run 'sempkg init' to create one.",
            workspace_dir.display()
        )
    })?;
    toml::from_str(&text).with_context(|| format!("Failed to parse {MANIFEST_FILE}"))
}

/// Write sempkg.toml using toml_edit so that dependency entries are serialized
/// as inline tables (`{ version = "1.0" }`) rather than dotted headers
/// (`[dependencies.pkg]`).
pub fn save_manifest(manifest: &WorkspaceManifest, workspace_dir: &Path) -> Result<()> {
    let path = workspace_dir.join(MANIFEST_FILE);
    let doc = build_document(manifest)?;
    let header = "# sempkg workspace manifest\n# Run 'sempkg sync' to install dependencies.\n\n";
    std::fs::write(&path, format!("{header}{doc}"))
        .with_context(|| format!("Failed to write {}", path.display()))
}

/// Build a toml_edit Document from a WorkspaceManifest.
fn build_document(manifest: &WorkspaceManifest) -> Result<DocumentMut> {
    let mut doc = DocumentMut::new();

    // [workspace]
    if let Some(ws) = &manifest.workspace {
        let mut t = Table::new();
        if let Some(k) = &ws.verify_key {
            t.insert("verify_key", value(k.as_str()));
        }
        doc.insert("workspace", Item::Table(t));
    }

    // [[registry]]
    if !manifest.registries.is_empty() {
        let arr = doc
            .entry("registry")
            .or_insert(Item::ArrayOfTables(toml_edit::ArrayOfTables::new()))
            .as_array_of_tables_mut()
            .context("registry must be an array of tables")?;
        for reg in &manifest.registries {
            let mut t = Table::new();
            t.insert("name", value(reg.name.as_str()));
            t.insert("url", value(reg.url.as_str()));
            arr.push(t);
        }
    }

    // [dependencies]
    if !manifest.dependencies.is_empty() {
        let mut t = Table::new();
        for (name, dep) in &manifest.dependencies {
            t.insert(name, Item::Value(Value::InlineTable(dep_inline(dep))));
        }
        doc.insert("dependencies", Item::Table(t));
    }

    // [dependency-groups]
    if !manifest.dependency_groups.is_empty() {
        let mut groups_table = Table::new();
        for (group_name, deps) in &manifest.dependency_groups {
            let mut group_inline = InlineTable::new();
            for (pkg_name, dep) in deps {
                group_inline.insert(pkg_name, Value::InlineTable(dep_inline(dep)));
            }
            groups_table.insert(group_name, Item::Value(Value::InlineTable(group_inline)));
        }
        doc.insert("dependency-groups", Item::Table(groups_table));
    }

    // [packages]
    if !manifest.packages.is_empty() {
        let mut t = Table::new();
        for (name, pkg) in &manifest.packages {
            let mut it = InlineTable::new();
            it.insert("path", Value::from(pkg.path.as_str()));
            if !pkg.description.is_empty() {
                it.insert("description", Value::from(pkg.description.as_str()));
            }
            t.insert(name, Item::Value(Value::InlineTable(it)));
        }
        doc.insert("packages", Item::Table(t));
    }

    // [reranker]
    if let Some(cfg) = &manifest.reranker {
        let mut t = Table::new();
        t.insert("enabled", value(cfg.enabled));
        if let Some(model) = &cfg.model {
            t.insert("model", value(model.as_str()));
        }
        t.insert("top_k", value(cfg.top_k as i64));
        t.insert("output_n", value(cfg.output_n as i64));
        doc.insert("reranker", Item::Table(t));
    }

    Ok(doc)
}

/// Build an inline table for a DependencyEntry: `{ version = "x", registry = "y" }`.
fn dep_inline(dep: &DependencyEntry) -> InlineTable {
    let mut it = InlineTable::new();
    it.insert("version", Value::from(dep.version.as_str()));
    if let Some(reg) = &dep.registry {
        it.insert("registry", Value::from(reg.as_str()));
    }
    if let Some(url) = &dep.url {
        it.insert("url", Value::from(url.as_str()));
    }
    if let Some(git) = &dep.git {
        it.insert("git", Value::from(git.as_str()));
    }
    if let Some(git_ref) = &dep.git_ref {
        it.insert("git_ref", Value::from(git_ref.as_str()));
    }
    if let Some(subdir) = &dep.subdir {
        it.insert("subdir", Value::from(subdir.as_str()));
    }
    if dep.full {
        it.insert("full", Value::from(true));
    }
    it
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
    let body = toml::to_string_pretty(lock).context("Failed to serialize lock file")?;
    std::fs::write(&path, format!("{header}{body}"))
        .with_context(|| format!("Failed to write {}", path.display()))
}

pub fn init_manifest(workspace_dir: &Path, registry_url: Option<&str>) -> Result<()> {
    let manifest_path = workspace_dir.join(MANIFEST_FILE);
    if manifest_path.exists() {
        anyhow::bail!("{MANIFEST_FILE} already exists at {}", workspace_dir.display());
    }

    let mut manifest = WorkspaceManifest::default();

    if let Some(url) = registry_url {
        manifest.registries.push(RegistryEntry {
            name: "default".to_string(),
            url: url.trim_end_matches('/').to_string(),
        });
    }

    save_manifest(&manifest, workspace_dir)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{DependencyEntry, WorkspaceManifest, load_manifest, save_manifest};

    #[test]
    fn save_manifest_preserves_reranker_table() {
        let dir = tempdir().expect("create temp dir");

        let mut manifest = WorkspaceManifest::default();
        manifest.dependencies.insert(
            "demo".to_string(),
            DependencyEntry {
                version: "1.0.0".to_string(),
                registry: None,
                url: None,
                git: None,
                git_ref: None,
                subdir: None,
                full: false,
            },
        );
        manifest.reranker = Some(crate::reranker::RerankerConfig {
            enabled: true,
            model: Some("~/.sempkg/models/custom.gguf".to_string()),
            top_k: 42,
            output_n: 7,
        });

        save_manifest(&manifest, dir.path()).expect("save manifest");

        let saved = fs::read_to_string(dir.path().join(super::MANIFEST_FILE))
            .expect("read manifest");
        assert!(saved.contains("[reranker]"));
        assert!(saved.contains("top_k = 42"));
        assert!(saved.contains("output_n = 7"));

        let loaded = load_manifest(dir.path()).expect("load manifest");
        let reranker = loaded.reranker.expect("reranker section present");
        assert!(reranker.enabled);
        assert_eq!(reranker.model.as_deref(), Some("~/.sempkg/models/custom.gguf"));
        assert_eq!(reranker.top_k, 42);
        assert_eq!(reranker.output_n, 7);
    }
}

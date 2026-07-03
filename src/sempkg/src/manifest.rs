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
use toml_edit::{value, DocumentMut, InlineTable, Item, Table, Value};

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
    /// Absolute path to a local folder used as the bundle source.
    /// Set when this dependency was added via `sempkg add <local-path>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local: Option<String>,
    /// When true, `sempkg sync` will rebuild the LanceDB source-code index
    /// (chunked by top-level symbols) when rebuilding this bundle.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub include_source: bool,
    /// Custom glob mask for the source-code index (only meaningful when
    /// `include_source` is true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_glob: Option<String>,
    /// Source directories to index with codegraph. Empty = use the bundle source root.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_dirs: Vec<String>,
    /// Documentation directories to index with LanceDB. Empty = use the bundle source root.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub docs_dirs: Vec<String>,
    /// Directories excluded from all indexing (source, docs, and source-code index).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude_dirs: Vec<String>,
    /// Optional one-line, user-supplied description of what this bundle provides.
    /// Set with `sempkg add --description`; surfaced in `sempkg list` and the MCP
    /// `list_packages` tool so agents can tell at a glance which package to search.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
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
    /// Optional vector-embedding configuration (semantic search).
    pub embedding: Option<crate::embedding::EmbeddingConfig>,
    /// Optional generative query-expansion configuration.
    pub query_expansion: Option<crate::query_expansion::QueryExpansionConfig>,
}

impl WorkspaceManifest {
    pub fn default_registry(&self) -> Option<&RegistryEntry> {
        self.registries.first()
    }

    pub fn get_registry(&self, name: &str) -> Option<&RegistryEntry> {
        self.registries.iter().find(|r| r.name == name)
    }

    /// Find a dependency by name across `[dependencies]` and every
    /// `[dependency-groups]` group. Returns the first match.
    pub fn find_dependency(&self, name: &str) -> Option<&DependencyEntry> {
        self.dependencies.get(name).or_else(|| {
            self.dependency_groups
                .values()
                .find_map(|group| group.get(name))
        })
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
    #[allow(dead_code)] // lookup helper kept as public API on the lock model
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
///
/// When possible, this preserves existing comments and formatting by loading the
/// existing document and updating only the changed sections.
pub fn save_manifest(manifest: &WorkspaceManifest, workspace_dir: &Path) -> Result<()> {
    let path = workspace_dir.join(MANIFEST_FILE);
    let header = "# sempkg workspace manifest\n# Run 'sempkg sync' to install dependencies.\n\n";

    // Try to load and preserve the existing document
    let mut doc = if path.exists() {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read existing {}", path.display()))?;
        // Strip the header if present so we can re-add it
        let text = if let Some(stripped) = text.strip_prefix(header) {
            stripped
        } else {
            text.as_str()
        };
        text.parse::<DocumentMut>()
            .unwrap_or_else(|_| DocumentMut::new())
    } else {
        DocumentMut::new()
    };

    // Update document with current manifest values
    update_document(&mut doc, manifest)?;

    std::fs::write(&path, format!("{header}{doc}"))
        .with_context(|| format!("Failed to write {}", path.display()))
}

/// Update a toml_edit Document with the current WorkspaceManifest values.
/// This preserves existing comments and formatting in the document.
///
/// Strategy:
/// - For dynamic/mutable sections (dependencies, registries, etc): rebuild from manifest
/// - For config sections ([embedding], [query_expansion], [reranker]): preserve existing
///   entries and comments, only update values from manifest if present
fn update_document(doc: &mut DocumentMut, manifest: &WorkspaceManifest) -> Result<()> {
    // [workspace]
    if let Some(ws) = &manifest.workspace {
        let mut t = Table::new();
        if let Some(k) = &ws.verify_key {
            t.insert("verify_key", value(k.as_str()));
        }
        doc.insert("workspace", Item::Table(t));
    } else {
        doc.remove("workspace");
    }

    // [[registry]]
    if !manifest.registries.is_empty() {
        let arr = doc
            .entry("registry")
            .or_insert(Item::ArrayOfTables(toml_edit::ArrayOfTables::new()))
            .as_array_of_tables_mut()
            .context("registry must be an array of tables")?;
        arr.clear();
        for reg in &manifest.registries {
            let mut t = Table::new();
            t.insert("name", value(reg.name.as_str()));
            t.insert("url", value(reg.url.as_str()));
            arr.push(t);
        }
    } else {
        doc.remove("registry");
    }

    // [dependencies]
    if !manifest.dependencies.is_empty() {
        let mut t = Table::new();
        for (name, dep) in &manifest.dependencies {
            t.insert(name, Item::Value(Value::InlineTable(dep_inline(dep))));
        }
        doc.insert("dependencies", Item::Table(t));
    } else {
        doc.remove("dependencies");
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
    } else {
        doc.remove("dependency-groups");
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
    } else {
        doc.remove("packages");
    }

    // [embedding] — preserve existing structure, update values from manifest
    if let Some(cfg) = &manifest.embedding {
        let mut t = if let Some(Item::Table(existing)) = doc.get("embedding") {
            existing.clone()
        } else {
            Table::new()
        };
        t.insert("enabled", value(cfg.enabled));
        if let Some(model) = &cfg.model {
            t.insert("model", value(model.as_str()));
        } else {
            t.remove("model");
        }
        t.insert("n_ctx", value(cfg.n_ctx as i64));
        t.insert("gpu_layers", value(cfg.gpu_layers as i64));
        doc.insert("embedding", Item::Table(t));
    }
    // Note: do NOT remove [embedding] if manifest.embedding is None —
    // this preserves manually configured sections

    // [query_expansion] — preserve existing structure, update values from manifest
    if let Some(cfg) = &manifest.query_expansion {
        let mut t = if let Some(Item::Table(existing)) = doc.get("query_expansion") {
            existing.clone()
        } else {
            Table::new()
        };
        t.insert("enabled", value(cfg.enabled));
        if let Some(model) = &cfg.model {
            t.insert("model", value(model.as_str()));
        } else {
            t.remove("model");
        }
        t.insert("max_variants", value(cfg.max_variants as i64));
        t.insert("max_tokens", value(cfg.max_tokens as i64));
        t.insert("n_ctx", value(cfg.n_ctx as i64));
        t.insert("gpu_layers", value(cfg.gpu_layers as i64));
        t.insert("temperature", value(cfg.temperature as f64));
        doc.insert("query_expansion", Item::Table(t));
    }
    // Note: do NOT remove [query_expansion] if manifest.query_expansion is None —
    // this preserves manually configured sections

    // [reranker] — preserve existing structure, update values from manifest
    if let Some(cfg) = &manifest.reranker {
        let mut t = if let Some(Item::Table(existing)) = doc.get("reranker") {
            existing.clone()
        } else {
            Table::new()
        };
        t.insert("enabled", value(cfg.enabled));
        if let Some(model) = &cfg.model {
            t.insert("model", value(model.as_str()));
        } else {
            t.remove("model");
        }
        t.insert("top_k", value(cfg.top_k as i64));
        t.insert("output_n", value(cfg.output_n as i64));
        doc.insert("reranker", Item::Table(t));
    }
    // Note: do NOT remove [reranker] if manifest.reranker is None —
    // this preserves manually configured sections

    Ok(())
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
    if let Some(local) = &dep.local {
        it.insert("local", Value::from(local.as_str()));
    }
    if dep.include_source {
        it.insert("include_source", Value::from(true));
    }
    if let Some(glob) = &dep.source_glob {
        it.insert("source_glob", Value::from(glob.as_str()));
    }
    if !dep.source_dirs.is_empty() {
        let mut arr = toml_edit::Array::new();
        for d in &dep.source_dirs {
            arr.push(d.as_str());
        }
        it.insert("source_dirs", Value::Array(arr));
    }
    if !dep.docs_dirs.is_empty() {
        let mut arr = toml_edit::Array::new();
        for d in &dep.docs_dirs {
            arr.push(d.as_str());
        }
        it.insert("docs_dirs", Value::Array(arr));
    }
    if !dep.exclude_dirs.is_empty() {
        let mut arr = toml_edit::Array::new();
        for d in &dep.exclude_dirs {
            arr.push(d.as_str());
        }
        it.insert("exclude_dirs", Value::Array(arr));
    }
    if let Some(desc) = &dep.description {
        it.insert("description", Value::from(desc.as_str()));
    }
    it
}

pub fn load_lock(workspace_dir: &Path) -> Result<LockFile> {
    let path = workspace_dir.join(LOCK_FILE);
    if !path.exists() {
        return Ok(LockFile::default());
    }
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("Failed to read {LOCK_FILE}"))?;
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
        anyhow::bail!(
            "{MANIFEST_FILE} already exists at {}",
            workspace_dir.display()
        );
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

    use super::{load_manifest, save_manifest, DependencyEntry, WorkspaceManifest};

    fn dep(version: &str) -> DependencyEntry {
        DependencyEntry {
            version: version.to_string(),
            registry: None,
            url: None,
            git: None,
            git_ref: None,
            subdir: None,
            full: false,
            local: None,
            include_source: false,
            source_glob: None,
            source_dirs: vec![],
            docs_dirs: vec![],
            exclude_dirs: vec![],
            description: None,
        }
    }

    #[test]
    fn dependency_description_round_trips() {
        let dir = tempdir().expect("create temp dir");

        let mut manifest = WorkspaceManifest::default();
        let mut described = dep("1.0.0");
        described.description = Some("HTTP client with retry support".to_string());
        manifest
            .dependencies
            .insert("reqwest".to_string(), described);
        // A dependency without a description must not emit the key.
        manifest
            .dependencies
            .insert("plain".to_string(), dep("2.0.0"));

        save_manifest(&manifest, dir.path()).expect("save manifest");

        let saved =
            fs::read_to_string(dir.path().join(super::MANIFEST_FILE)).expect("read manifest");
        assert!(saved.contains("description = \"HTTP client with retry support\""));
        // Only one description line should be present (the plain dep omits it).
        assert_eq!(saved.matches("description =").count(), 1);

        let loaded = load_manifest(dir.path()).expect("load manifest");
        assert_eq!(
            loaded
                .find_dependency("reqwest")
                .and_then(|d| d.description.as_deref()),
            Some("HTTP client with retry support")
        );
        assert_eq!(
            loaded
                .find_dependency("plain")
                .and_then(|d| d.description.clone()),
            None
        );
    }

    #[test]
    fn find_dependency_searches_groups() {
        let mut manifest = WorkspaceManifest::default();
        let mut grouped = dep("3.0.0");
        grouped.description = Some("test-only helper".to_string());
        manifest
            .dependency_groups
            .entry("dev".to_string())
            .or_default()
            .insert("pytest".to_string(), grouped);

        assert_eq!(
            manifest
                .find_dependency("pytest")
                .and_then(|d| d.description.as_deref()),
            Some("test-only helper")
        );
        assert!(manifest.find_dependency("missing").is_none());
    }

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
                local: None,
                include_source: false,
                source_glob: None,
                source_dirs: vec![],
                docs_dirs: vec![],
                exclude_dirs: vec![],
                description: None,
            },
        );
        manifest.reranker = Some(crate::reranker::RerankerConfig {
            enabled: true,
            model: Some("~/.sempkg/models/custom.gguf".to_string()),
            top_k: 42,
            output_n: 7,
            ..Default::default()
        });

        save_manifest(&manifest, dir.path()).expect("save manifest");

        let saved =
            fs::read_to_string(dir.path().join(super::MANIFEST_FILE)).expect("read manifest");
        assert!(saved.contains("[reranker]"));
        assert!(saved.contains("top_k = 42"));
        assert!(saved.contains("output_n = 7"));

        let loaded = load_manifest(dir.path()).expect("load manifest");
        let reranker = loaded.reranker.expect("reranker section present");
        assert!(reranker.enabled);
        assert_eq!(
            reranker.model.as_deref(),
            Some("~/.sempkg/models/custom.gguf")
        );
        assert_eq!(reranker.top_k, 42);
        assert_eq!(reranker.output_n, 7);
    }

    #[test]
    fn save_manifest_preserves_embedding_table() {
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
                local: None,
                include_source: false,
                source_glob: None,
                source_dirs: vec![],
                docs_dirs: vec![],
                exclude_dirs: vec![],
                description: None,
            },
        );
        manifest.embedding = Some(crate::embedding::EmbeddingConfig {
            enabled: true,
            model_id: "qwen3-embedding-0.6b".to_string(),
            model: Some("~/.sempkg/models/custom-embedding.gguf".to_string()),
            n_ctx: 1024,
            gpu_layers: 2,
            ..Default::default()
        });

        save_manifest(&manifest, dir.path()).expect("save manifest");

        let saved =
            fs::read_to_string(dir.path().join(super::MANIFEST_FILE)).expect("read manifest");
        assert!(saved.contains("[embedding]"));
        assert!(saved.contains("n_ctx = 1024"));
        assert!(saved.contains("gpu_layers = 2"));

        let loaded = load_manifest(dir.path()).expect("load manifest");
        let embedding = loaded.embedding.expect("embedding section present");
        assert!(embedding.enabled);
        assert_eq!(
            embedding.model.as_deref(),
            Some("~/.sempkg/models/custom-embedding.gguf")
        );
        assert_eq!(embedding.n_ctx, 1024);
        assert_eq!(embedding.gpu_layers, 2);
    }

    #[test]
    fn save_manifest_preserves_query_expansion_table() {
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
                local: None,
                include_source: false,
                source_glob: None,
                source_dirs: vec![],
                docs_dirs: vec![],
                exclude_dirs: vec![],
                description: None,
            },
        );
        manifest.query_expansion = Some(crate::query_expansion::QueryExpansionConfig {
            enabled: true,
            model: Some("~/.sempkg/models/custom-expansion.gguf".to_string()),
            max_variants: 8,
            max_tokens: 512,
            n_ctx: 4096,
            gpu_layers: 4,
            temperature: 0.5,
            ..Default::default()
        });

        save_manifest(&manifest, dir.path()).expect("save manifest");

        let saved =
            fs::read_to_string(dir.path().join(super::MANIFEST_FILE)).expect("read manifest");
        assert!(saved.contains("[query_expansion]"));
        assert!(saved.contains("max_variants = 8"));
        assert!(saved.contains("max_tokens = 512"));
        assert!(saved.contains("n_ctx = 4096"));
        assert!(saved.contains("gpu_layers = 4"));

        let loaded = load_manifest(dir.path()).expect("load manifest");
        let expansion = loaded
            .query_expansion
            .expect("query_expansion section present");
        assert!(expansion.enabled);
        assert_eq!(
            expansion.model.as_deref(),
            Some("~/.sempkg/models/custom-expansion.gguf")
        );
        assert_eq!(expansion.max_variants, 8);
        assert_eq!(expansion.max_tokens, 512);
        assert_eq!(expansion.n_ctx, 4096);
        assert_eq!(expansion.gpu_layers, 4);
        assert_eq!(expansion.temperature, 0.5);
    }

    #[test]
    fn save_manifest_preserves_all_config_tables() {
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
                local: None,
                include_source: false,
                source_glob: None,
                source_dirs: vec![],
                docs_dirs: vec![],
                exclude_dirs: vec![],
                description: None,
            },
        );
        manifest.embedding = Some(crate::embedding::EmbeddingConfig {
            enabled: true,
            model_id: "embeddinggemma-300m".to_string(),
            model: None,
            n_ctx: 2048,
            gpu_layers: 0,
            ..Default::default()
        });
        manifest.query_expansion = Some(crate::query_expansion::QueryExpansionConfig {
            enabled: true,
            model: None,
            max_variants: 4,
            max_tokens: 256,
            n_ctx: 2048,
            gpu_layers: 0,
            temperature: 0.7,
            ..Default::default()
        });
        manifest.reranker = Some(crate::reranker::RerankerConfig {
            enabled: true,
            model: None,
            top_k: 20,
            output_n: 5,
            ..Default::default()
        });

        save_manifest(&manifest, dir.path()).expect("save manifest");

        let saved =
            fs::read_to_string(dir.path().join(super::MANIFEST_FILE)).expect("read manifest");
        assert!(saved.contains("[embedding]"));
        assert!(saved.contains("[query_expansion]"));
        assert!(saved.contains("[reranker]"));

        let loaded = load_manifest(dir.path()).expect("load manifest");
        assert!(loaded.embedding.is_some());
        assert!(loaded.query_expansion.is_some());
        assert!(loaded.reranker.is_some());
    }
}

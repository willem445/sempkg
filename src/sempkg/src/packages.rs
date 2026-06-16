/// Local package registry — tracks locally cloned repos for codegraph indexing.
///
/// Stored at `~/.sempkg/packages.json`.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::codegraph;

fn registry_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".sempkg")
        .join("packages.json")
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LocalPackage {
    pub name: String,
    pub path: String,
    #[serde(default)]
    pub description: String,
}

impl LocalPackage {
    pub fn abs_path(&self) -> PathBuf {
        PathBuf::from(&self.path)
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(&self.path))
    }

    pub fn is_indexed(&self) -> bool {
        codegraph::is_indexed(&self.abs_path())
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct PackageRegistry {
    packages: BTreeMap<String, LocalPackage>,
}

impl PackageRegistry {
    pub fn load() -> Result<Self> {
        let path = registry_path();
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        let map: BTreeMap<String, LocalPackage> =
            serde_json::from_str(&text).context("Failed to parse packages.json")?;
        Ok(Self { packages: map })
    }

    fn save(&self) -> Result<()> {
        let path = registry_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Cannot create directory: {}", parent.display()))?;
        }
        let text = serde_json::to_string_pretty(&self.packages)
            .context("Failed to serialize packages")?;
        std::fs::write(&path, text)
            .with_context(|| format!("Failed to write {}", path.display()))
    }

    pub fn add(&mut self, name: &str, path: &Path, description: &str) -> Result<&LocalPackage> {
        let abs = path
            .canonicalize()
            .with_context(|| format!("Path does not exist: {}", path.display()))?;

        let pkg = LocalPackage {
            name: name.to_string(),
            path: abs.to_string_lossy().to_string(),
            description: description.to_string(),
        };
        self.packages.insert(name.to_string(), pkg);
        self.save()?;
        Ok(self.packages.get(name).unwrap())
    }

    pub fn remove(&mut self, name: &str) -> Result<bool> {
        if self.packages.remove(name).is_some() {
            self.save()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn get(&self, name: &str) -> Option<&LocalPackage> {
        self.packages.get(name)
    }

    pub fn list(&self) -> Vec<&LocalPackage> {
        self.packages.values().collect()
    }
}

/// Registry HTTP client — fetches bundle index and downloads archives.
use std::collections::HashMap;

use anyhow::{Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::error::SempkgError;

// ---------------------------------------------------------------------------
// Registry index schema
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct RegistryIndex {
    pub packages: HashMap<String, RegistryPackage>,
}

#[derive(Debug, Deserialize)]
pub struct RegistryPackage {
    pub bundles: HashMap<String, RegistryBundle>,
}

#[derive(Debug, Deserialize)]
pub struct RegistryBundle {
    pub sha256: Option<String>,
    #[serde(default)]
    pub signed: bool,
    pub url: Option<String>,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

pub struct RegistryClient {
    base_url: String,
    client: reqwest::blocking::Client,
}

impl RegistryClient {
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client: reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .expect("Failed to build HTTP client"),
        }
    }

    /// Fetch and parse `<base>/index.json`.
    pub fn fetch_index(&self) -> Result<RegistryIndex> {
        let url = format!("{}/index.json", self.base_url);
        let resp = self
            .client
            .get(&url)
            .send()
            .with_context(|| SempkgError::RegistryError {
                url: url.clone(),
                message: "Failed to connect".to_string(),
            })?;

        if !resp.status().is_success() {
            return Err(SempkgError::RegistryError {
                url,
                message: format!("HTTP {}", resp.status()),
            }
            .into());
        }

        resp.json::<RegistryIndex>()
            .context("Failed to parse registry index")
    }

    /// Download a bundle archive, verify SHA-256, and return raw bytes.
    pub fn download_bundle(
        &self,
        package: &str,
        version: &str,
        expected_sha256: Option<&str>,
    ) -> Result<Vec<u8>> {
        let url = format!(
            "{}/bundles/{}/{}/{}-{}.sembundle",
            self.base_url, package, version, package, version
        );

        let resp = self
            .client
            .get(&url)
            .send()
            .with_context(|| format!("Failed to download bundle from {url}"))?;

        if !resp.status().is_success() {
            return Err(SempkgError::RegistryError {
                url,
                message: format!("HTTP {}", resp.status()),
            }
            .into());
        }

        let bytes = resp
            .bytes()
            .context("Failed to read bundle response body")?;
        let bytes = bytes.to_vec();

        if let Some(expected) = expected_sha256 {
            let actual = hex::encode(Sha256::digest(&bytes));
            if actual != expected {
                return Err(SempkgError::ChecksumMismatch {
                    path: format!("{package}-{version}.sembundle"),
                    expected: expected.to_string(),
                    actual,
                }
                .into());
            }
        }

        Ok(bytes)
    }

    /// Download the Ed25519 signature file for a bundle.
    pub fn download_signature(&self, package: &str, version: &str) -> Result<Vec<u8>> {
        let url = format!(
            "{}/bundles/{}/{}/{}-{}.sembundle.sig",
            self.base_url, package, version, package, version
        );

        let resp = self
            .client
            .get(&url)
            .send()
            .with_context(|| format!("Failed to download signature from {url}"))?;

        if !resp.status().is_success() {
            return Err(SempkgError::RegistryError {
                url,
                message: format!("HTTP {}", resp.status()),
            }
            .into());
        }

        let bytes = resp.bytes().context("Failed to read signature")?;
        Ok(bytes.to_vec())
    }

    /// Lookup a specific bundle entry in the registry index.
    pub fn lookup(&self, package: &str, version: &str) -> Result<RegistryBundle> {
        let index = self.fetch_index()?;
        index
            .packages
            .get(package)
            .and_then(|p| p.bundles.get(version))
            .map(|b| RegistryBundle {
                sha256: b.sha256.clone(),
                signed: b.signed,
                url: b.url.clone(),
            })
            .ok_or_else(|| {
                SempkgError::BundleNotFound {
                    name: package.to_string(),
                    version: version.to_string(),
                }
                .into()
            })
    }

    /// List all packages and versions available in the registry.
    pub fn list_available(&self) -> Result<Vec<(String, String)>> {
        let index = self.fetch_index()?;
        let mut entries = Vec::new();
        for (pkg_name, pkg) in &index.packages {
            for version in pkg.bundles.keys() {
                entries.push((pkg_name.clone(), version.clone()));
            }
        }
        entries.sort();
        Ok(entries)
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}

// ---------------------------------------------------------------------------
// Standalone URL download (GitHub releases or any direct .sembundle URL)
// ---------------------------------------------------------------------------

/// Download a `.sembundle` from an arbitrary URL and optionally verify its SHA-256.
///
/// Use this when a dependency specifies a `url` directly (e.g. a GitHub release
/// asset) rather than going through a registry.
pub fn download_from_url(url: &str, expected_sha256: Option<&str>) -> Result<Vec<u8>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .context("Failed to build HTTP client")?;

    let resp = client
        .get(url)
        .send()
        .with_context(|| format!("Failed to connect to {url}"))?;

    if !resp.status().is_success() {
        return Err(SempkgError::RegistryError {
            url: url.to_string(),
            message: format!("HTTP {}", resp.status()),
        }
        .into());
    }

    let bytes = resp
        .bytes()
        .context("Failed to read response body")?
        .to_vec();

    if let Some(expected) = expected_sha256 {
        let actual = hex::encode(Sha256::digest(&bytes));
        if actual != expected {
            return Err(SempkgError::ChecksumMismatch {
                path: url.to_string(),
                expected: expected.to_string(),
                actual,
            }
            .into());
        }
    }

    Ok(bytes)
}

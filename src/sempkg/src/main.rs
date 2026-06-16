mod cli;
mod codegraph;
mod error;
mod manifest;
mod mcp;
mod packages;
mod qmd;
mod registry;
mod store;
mod verify;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;

use sha2::{Digest, Sha256};

use crate::cli::{Cli, Commands, PkgCommands};
use crate::manifest::{DependencyEntry, RegistryEntry};
use crate::packages::PackageRegistry;
use crate::registry::RegistryClient;
use crate::store::{BundleScope, BundleStore, list_all_bundles, repair_codegraph_view, resolve_bundle};

fn main() {
    let cli = Cli::parse();
    let workspace = resolve_workspace(cli.workspace.as_deref());

    if let Err(e) = run(cli.command, workspace.as_deref()) {
        eprintln!("Error: {e:#}");
        std::process::exit(1);
    }
}

/// Resolve workspace directory: use provided path, or fall back to cwd.
fn resolve_workspace(override_dir: Option<&Path>) -> Option<PathBuf> {
    override_dir
        .map(|p| p.to_path_buf())
        .or_else(|| std::env::current_dir().ok())
}

fn run(cmd: Commands, workspace: Option<&Path>) -> Result<()> {
    match cmd {
        // -----------------------------------------------------------------------
        // Workspace initialisation
        // -----------------------------------------------------------------------
        Commands::Init { registry } => {
            let dir = workspace.unwrap_or(Path::new("."));
            manifest::init_manifest(dir, registry.as_deref())?;
            println!("Created {}", dir.join(manifest::MANIFEST_FILE).display());
            if registry.is_some() {
                println!("Add dependencies with: sempkg add <name>@<version>");
            } else {
                println!("Add a registry with: sempkg add --registry-url <url> <name>@<version>");
            }
            Ok(())
        }

        // -----------------------------------------------------------------------
        // List
        // -----------------------------------------------------------------------
        Commands::List => {
            let registry = PackageRegistry::load()?;
            let local_pkgs = registry.list();
            let bundles = list_all_bundles(workspace);

            if local_pkgs.is_empty() && bundles.is_empty() {
                println!("No packages or bundles registered.");
                println!("  Local packages: sempkg pkg add <name> <path>");
                println!("  Bundles:        sempkg sync  or  sempkg install <name>@<version> --registry <url>");
                return Ok(());
            }

            if !local_pkgs.is_empty() {
                println!("Local packages:");
                for pkg in &local_pkgs {
                    let idx = if pkg.is_indexed() { "indexed" } else { "NOT indexed" };
                    let desc = if pkg.description.is_empty() {
                        String::new()
                    } else {
                        format!("  # {}", pkg.description)
                    };
                    println!("  {:<24} [{idx}]  {}{desc}", pkg.name, pkg.path);
                }
            }

            if !bundles.is_empty() {
                if !local_pkgs.is_empty() { println!(); }
                println!("Installed bundles:");
                for b in &bundles {
                    let idx = if b.is_indexed() { "indexed" } else { "no graph" };
                    let qmd_flag = if b.has_qmd() { "  +qmd" } else { "" };
                    let scope = match b.scope {
                        BundleScope::Workspace => "workspace",
                        BundleScope::Global    => "global",
                    };
                    println!(
                        "  {:<20} @ {:<12} [{idx}]  [{scope}]{qmd_flag}",
                        b.name, b.version
                    );
                }
            }

            Ok(())
        }

        // -----------------------------------------------------------------------
        // Add dependency to manifest
        // -----------------------------------------------------------------------
        Commands::Add { spec, registry_url, registry } => {
            let dir = require_workspace(workspace)?;
            let (name, version) = parse_spec(&spec)?;

            let mut mf = manifest::load_manifest(dir)?;

            // Add or update registry if a URL was provided
            if let Some(url) = &registry_url {
                let url = url.trim_end_matches('/');
                let reg_name = registry.clone().unwrap_or_else(|| "default".to_string());
                if mf.get_registry(&reg_name).is_none() {
                    mf.registries.push(RegistryEntry {
                        name: reg_name.clone(),
                        url: url.to_string(),
                    });
                }
                mf.dependencies.insert(
                    name.to_string(),
                    DependencyEntry {
                        version: version.to_string(),
                        registry: Some(reg_name),
                    },
                );
            } else {
                if mf.registries.is_empty() {
                    anyhow::bail!(
                        "No registries defined in sempkg.toml. \
                         Use --registry-url to add one: sempkg add --registry-url <url> {spec}"
                    );
                }
                mf.dependencies.insert(
                    name.to_string(),
                    DependencyEntry {
                        version: version.to_string(),
                        registry: registry,
                    },
                );
            }

            manifest::save_manifest(&mf, dir)?;
            println!("Added {name}@{version} to sempkg.toml. Run 'sempkg sync' to install.");
            Ok(())
        }

        // -----------------------------------------------------------------------
        // Remove dependency from manifest
        // -----------------------------------------------------------------------
        Commands::Remove { name } => {
            let dir = require_workspace(workspace)?;
            let mut mf = manifest::load_manifest(dir)?;
            if mf.dependencies.remove(&name).is_some() {
                manifest::save_manifest(&mf, dir)?;
                println!("Removed '{name}' from sempkg.toml.");
            } else {
                anyhow::bail!("'{name}' is not in sempkg.toml dependencies.");
            }
            Ok(())
        }

        // -----------------------------------------------------------------------
        // Sync — install all workspace manifest dependencies
        // -----------------------------------------------------------------------
        Commands::Sync { reinstall } => {
            let dir = require_workspace(workspace)?;
            let mf = manifest::load_manifest(dir)?;
            let mut lock = manifest::load_lock(dir)?;
            let store = BundleStore::workspace(dir);
            let verify_key_path = mf.verify_key_path(dir);

            if mf.dependencies.is_empty() {
                println!("No dependencies in sempkg.toml.");
                return Ok(());
            }

            let verify_key = verify_key_path
                .as_deref()
                .map(verify::load_verifying_key)
                .transpose()?;

            let mut installed = Vec::new();

            for (dep_name, dep) in &mf.dependencies {
                if !reinstall && store.is_installed(dep_name, &dep.version) {
                    println!("  {dep_name}@{} already installed, skipping.", dep.version);
                    continue;
                }

                let reg = mf.registry_for(dep).with_context(|| {
                    format!("No registry found for dependency '{dep_name}'")
                })?;

                print!("  Installing {dep_name}@{} from {} ... ", dep.version, reg.name);
                std::io::Write::flush(&mut std::io::stdout())?;

                let client = RegistryClient::new(&reg.url);
                let index_entry = client.lookup(dep_name, &dep.version).ok();
                let expected_sha256 = index_entry.as_ref().and_then(|e| e.sha256.as_deref());

                let bytes = client.download_bundle(dep_name, &dep.version, expected_sha256)?;

                // Signature verification
                if let Some(key) = &verify_key {
                    let sig = client.download_signature(dep_name, &dep.version)?;
                    verify::verify_bundle_signature(&bytes, &sig, key)
                        .with_context(|| format!("Signature verification failed for {dep_name}@{}", dep.version))?;
                }

                let info = store.install_bytes(&bytes)?;
                println!("done.");

                lock.upsert(manifest::LockEntry {
                    name: dep_name.clone(),
                    version: dep.version.clone(),
                    registry_url: reg.url.clone(),
                    sha256: hex::encode(Sha256::digest(&bytes)),
                    signed: verify_key.is_some(),
                    manifest_checksums: info.manifest.checksums.clone(),
                });
                installed.push(format!("{dep_name}@{}", dep.version));
            }

            manifest::save_lock(&lock, dir)?;

            if installed.is_empty() {
                println!("All dependencies already installed.");
            } else {
                println!("Installed: {}", installed.join(", "));
            }

            Ok(())
        }

        // -----------------------------------------------------------------------
        // Install — direct download without manifest
        // -----------------------------------------------------------------------
        Commands::Install { spec, global, registry: reg_url, verify_key } => {
            let (name, version) = parse_spec(&spec)?;

            let store = if global {
                BundleStore::global()
            } else {
                BundleStore::workspace(require_workspace(workspace)?)
            };

            let scope_label = if global { "global" } else { "workspace" };
            let client = RegistryClient::new(&reg_url);

            let index_entry = client.lookup(name, version).ok();
            let expected_sha256 = index_entry.as_ref().and_then(|e| e.sha256.as_deref());

            print!("Downloading {name}@{version} ... ");
            std::io::Write::flush(&mut std::io::stdout())?;
            let bytes = client.download_bundle(name, version, expected_sha256)?;
            println!("done.");

            // Signature verification
            if let Some(key_path) = &verify_key {
                let key = verify::load_verifying_key(key_path)?;
                let sig = client.download_signature(name, version)?;
                verify::verify_bundle_signature(&bytes, &sig, &key)
                    .context("Signature verification failed")?;
                println!("Signature verified.");
            }

            let info = store.install_bytes(&bytes)?;
            println!(
                "Installed {}@{} [{scope_label}]{}",
                info.name, info.version,
                if info.has_qmd() { "  +qmd" } else { "" }
            );
            Ok(())
        }

        // -----------------------------------------------------------------------
        // Status
        // -----------------------------------------------------------------------
        Commands::Status { name } => {
            let reg = PackageRegistry::load()?;
            if let Some(pkg) = reg.get(&name) {
                println!("Package: {} (local)", pkg.name);
                println!("  Path:    {}", pkg.path);
                println!("  Indexed: {}", pkg.is_indexed());
                if pkg.is_indexed() {
                    match codegraph::status(&pkg.abs_path()) {
                        Ok(s) => println!("\n{s}"),
                        Err(e) => println!("  codegraph status error: {e}"),
                    }
                }
                return Ok(());
            }

            if let Some(bundle) = resolve_bundle(&name, workspace) {
                println!("Bundle: {}@{}", bundle.name, bundle.version);
                println!("  Path:       {}", bundle.bundle_dir.display());
                println!("  Scope:      {:?}", bundle.scope);
                println!("  Graph:      {}", bundle.bundle_dir.join("graph").exists());
                println!("  .codegraph: {}", bundle.bundle_dir.join(".codegraph").exists());
                println!("  Queryable:  {}", bundle.is_indexed());
                println!("  QMD:        {}", bundle.has_qmd());
                println!("  Source:     {}", bundle.manifest.source_repo);
                println!("  Commit:     {}", bundle.manifest.commit_hash);
                println!("  Created:    {}", bundle.manifest.created_at);
                if bundle.bundle_dir.join("graph").exists() && !bundle.bundle_dir.join(".codegraph").exists() {
                    println!("\n  ! .codegraph view missing — run 'sempkg repair' to fix.");
                }
                return Ok(());
            }

            anyhow::bail!("'{name}' not found. Run 'sempkg list' to see available packages.");
        }

        Commands::Repair => {
            let bundles = list_all_bundles(workspace);
            if bundles.is_empty() {
                println!("No bundles installed.");
                return Ok(());
            }
            let mut repaired = 0usize;
            let mut already_ok = 0usize;
            for bundle in &bundles {
                match repair_codegraph_view(&bundle.bundle_dir) {
                    Ok(true) => {
                        println!("  Repaired: {}@{}", bundle.name, bundle.version);
                        repaired += 1;
                    }
                    Ok(false) => { already_ok += 1; }
                    Err(e) => eprintln!("  Failed {}@{}: {e}", bundle.name, bundle.version),
                }
            }
            println!("\n{repaired} repaired, {already_ok} already OK.");
            Ok(())
        }

        // -----------------------------------------------------------------------
        // Local package commands
        // -----------------------------------------------------------------------
        Commands::Pkg(pkg_cmd) => run_pkg(pkg_cmd),

        // -----------------------------------------------------------------------
        // CodeGraph queries (scoped)
        // -----------------------------------------------------------------------
        Commands::Search { package, query, kind, limit } => {
            let path = resolve_codegraph_path(&package, workspace)?;
            println!("{}", codegraph::query(&path, &query, kind.as_deref(), limit)?);
            Ok(())
        }

        Commands::Callers { package, symbol, limit } => {
            let path = resolve_codegraph_path(&package, workspace)?;
            println!("{}", codegraph::callers(&path, &symbol, limit)?);
            Ok(())
        }

        Commands::Callees { package, symbol, limit } => {
            let path = resolve_codegraph_path(&package, workspace)?;
            println!("{}", codegraph::callees(&path, &symbol, limit)?);
            Ok(())
        }

        Commands::Context { package, task } => {
            let path = resolve_codegraph_path(&package, workspace)?;
            println!("{}", codegraph::context(&path, &task)?);
            Ok(())
        }

        Commands::Impact { package, symbol, depth } => {
            let path = resolve_codegraph_path(&package, workspace)?;
            println!("{}", codegraph::impact(&path, &symbol, depth)?);
            Ok(())
        }

        Commands::Files { package, filter } => {
            let path = resolve_codegraph_path(&package, workspace)?;
            println!("{}", codegraph::files(&path, filter.as_deref())?);
            Ok(())
        }

        // -----------------------------------------------------------------------
        // QMD doc search
        // -----------------------------------------------------------------------
        Commands::Docs { package, query, limit } => {
            let (bundle_dir, collection) = resolve_qmd_path(&package, workspace)?;
            let results = qmd::search(&bundle_dir, &query, limit, Some(&collection))?;
            println!("{}", qmd::format_results(&results, &query));
            Ok(())
        }

        Commands::DocsMeta { package } => {
            let (bundle_dir, _) = resolve_qmd_path(&package, workspace)?;
            if let Some(meta) = qmd::load_metadata(&bundle_dir) {
                println!("QMD metadata for '{package}':");
                println!("  Version:         {}", meta.qmd_version.as_deref().unwrap_or("unknown"));
                println!("  Model:           {}", meta.embed_model.as_deref().unwrap_or("unknown"));
                println!("  Chunk strategy:  {}", meta.chunk_strategy.as_deref().unwrap_or("unknown"));
                println!("  Collection:      {}", meta.collection_name.as_deref().unwrap_or("unknown"));
                println!("  Documents:       {}", meta.document_count.unwrap_or(0));
                println!("  Chunks:          {}", meta.chunk_count.unwrap_or(0));
                println!("  Indexed at:      {}", meta.created_at.as_deref().unwrap_or("unknown"));
            } else {
                anyhow::bail!("No QMD metadata found for '{package}'.");
            }
            Ok(())
        }

        // -----------------------------------------------------------------------
        // MCP server
        // -----------------------------------------------------------------------
        Commands::Mcp => {
            let ws = workspace.map(|p| p.to_path_buf());
            mcp::run(ws)?;
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// pkg sub-commands
// ---------------------------------------------------------------------------

fn run_pkg(cmd: PkgCommands) -> Result<()> {
    match cmd {
        PkgCommands::List => {
            let reg = PackageRegistry::load()?;
            let pkgs = reg.list();
            if pkgs.is_empty() {
                println!("No local packages registered.");
                println!("  Use: sempkg pkg add <name> <path>");
            } else {
                println!("Local packages:");
                for pkg in pkgs {
                    let idx = if pkg.is_indexed() { "indexed" } else { "NOT indexed" };
                    let desc = if pkg.description.is_empty() {
                        String::new()
                    } else {
                        format!("  # {}", pkg.description)
                    };
                    println!("  {:<24} [{idx}]  {}{desc}", pkg.name, pkg.path);
                }
            }
            Ok(())
        }

        PkgCommands::Add { name, path, description } => {
            let mut reg = PackageRegistry::load()?;
            reg.add(&name, &path, &description)?;
            let pkg = reg.get(&name).unwrap();
            if pkg.is_indexed() {
                println!("Registered '{name}' (already indexed).");
                println!("  Run 'sempkg pkg reindex {name}' to refresh.");
            } else {
                print!("Registered '{name}'. Indexing {} ... ", pkg.path);
                std::io::Write::flush(&mut std::io::stdout())?;
                match codegraph::init_and_index(&pkg.abs_path()) {
                    Ok(out) => {
                        println!("done.");
                        if !out.is_empty() { println!("{out}"); }
                    }
                    Err(e) => {
                        println!("failed.");
                        eprintln!("Indexing error: {e}");
                        eprintln!("Fix the error then run: sempkg pkg reindex {name}");
                    }
                }
            }
            Ok(())
        }

        PkgCommands::Remove { name } => {
            let mut reg = PackageRegistry::load()?;
            if reg.remove(&name)? {
                println!("Removed '{name}' (repo and index files untouched).");
            } else {
                anyhow::bail!("Package '{name}' not found.");
            }
            Ok(())
        }

        PkgCommands::Reindex { name } => {
            let reg = PackageRegistry::load()?;
            let pkg = reg.get(&name).with_context(|| format!("Package '{name}' not found."))?;
            print!("Reindexing '{}' ... ", pkg.path);
            std::io::Write::flush(&mut std::io::stdout())?;
            let out = if pkg.is_indexed() {
                codegraph::sync(&pkg.abs_path())?
            } else {
                codegraph::init_and_index(&pkg.abs_path())?
            };
            println!("done.");
            if !out.is_empty() { println!("{out}"); }
            Ok(())
        }

        PkgCommands::Status { name } => {
            let reg = PackageRegistry::load()?;
            let pkg = reg.get(&name).with_context(|| format!("Package '{name}' not found."))?;
            println!("{}", codegraph::status(&pkg.abs_path())?);
            Ok(())
        }

        PkgCommands::QmdIndex { name, pattern } => {
            let reg = PackageRegistry::load()?;
            let pkg = reg.get(&name).with_context(|| format!("Package '{name}' not found."))?;
            println!(
                "Building QMD index for '{name}' (pattern: {pattern})\n\
                 Index stored at: {}/.sempkg/qmd/index.sqlite\n\
                 Global QMD collections are not affected.",
                pkg.path
            );
            let db_path = qmd::cli_update(&pkg.abs_path(), &pkg.name, &pattern)?;
            println!("QMD index ready at {}", db_path.display());
            println!(
                "Search it with: sempkg docs {name} \"<query>\"\n\
                 (or via MCP tool: search_docs)"
            );
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn require_workspace(workspace: Option<&Path>) -> Result<&Path> {
    workspace.ok_or_else(|| anyhow::anyhow!("Could not determine workspace directory."))
}

/// Parse `name@version` spec.
fn parse_spec(spec: &str) -> Result<(&str, &str)> {
    spec.split_once('@')
        .ok_or_else(|| anyhow::anyhow!("Invalid spec '{spec}'. Expected format: name@version"))
}

/// Resolve a package name to its codegraph project path.
fn resolve_codegraph_path(name: &str, workspace: Option<&Path>) -> Result<PathBuf> {
    let reg = PackageRegistry::load()?;

    if let Some(pkg) = reg.get(name) {
        if !pkg.is_indexed() {
            anyhow::bail!(
                "Package '{name}' is not indexed. Run 'sempkg pkg reindex {name}' first."
            );
        }
        return Ok(pkg.abs_path());
    }

    if let Some(bundle) = resolve_bundle(name, workspace) {
        if !bundle.is_indexed() {
            anyhow::bail!(
                "Bundle '{name}@{}' has no codegraph index.",
                bundle.version
            );
        }
        return Ok(bundle.bundle_dir);
    }

    let reg2 = PackageRegistry::load()?;
    let mut names: Vec<String> = reg2.list().iter().map(|p| p.name.clone()).collect();
    names.extend(
        list_all_bundles(workspace)
            .iter()
            .map(|b| format!("{}@{}", b.name, b.version)),
    );
    let hint = if names.is_empty() {
        " No packages or bundles available.".to_string()
    } else {
        format!(" Available: {}", names.join(", "))
    };
    anyhow::bail!("Package '{name}' not found.{hint}")
}

/// Resolve a name to its QMD-queryable directory, and its collection name.
/// Returns `(bundle_dir, collection_name)`.
/// Checks local packages first, then installed bundles.
fn resolve_qmd_path(name: &str, workspace: Option<&Path>) -> Result<(PathBuf, String)> {
    // Local package with a scoped QMD index built by `sempkg pkg qmd-index`
    // Layout: <pkg-path>/.sempkg/qmd/index/index.sqlite
    // qmd_db_path() appends qmd/index/index.sqlite, so bundle_dir must be <pkg-path>/.sempkg/
    let reg = PackageRegistry::load()?;
    if let Some(pkg) = reg.get(name) {
        let sempkg_dir = pkg.abs_path().join(".sempkg");
        let local_db = sempkg_dir.join("qmd").join("index").join("index.sqlite");
        if local_db.exists() {
            return Ok((sempkg_dir, pkg.name.clone()));
        }
        anyhow::bail!(
            "Package '{name}' has no QMD index. Run 'sempkg pkg qmd-index {name}' to build one."
        );
    }

    // Installed bundle (pre-built QMD index inside the bundle)
    if let Some(bundle) = resolve_bundle(name, workspace) {
        if !bundle.has_qmd() {
            anyhow::bail!(
                "Bundle '{name}@{}' does not have a QMD documentation index.",
                bundle.version
            );
        }
        return Ok((bundle.bundle_dir, name.to_string()));
    }

    anyhow::bail!("'{name}' not found. Run 'sempkg list' to see available packages and bundles.")
}

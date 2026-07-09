mod accel;
mod cli;
mod codegraph;
mod embedding;
mod error;
mod github;
mod graph;
mod lance;
#[cfg(any(feature = "reranker", feature = "embeddings"))]
mod llama_runtime;
mod manifest;
mod mcp;
mod packages;
mod providers;
mod query_expansion;
mod registry;
mod reranker;
mod store;
mod verify;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;

use crate::cli::{
    Cli, Commands, EmbeddingCommands, PkgCommands, QueryExpansionCommands, RerankerCommands,
};
use crate::manifest::{DependencyEntry, RegistryEntry};
use crate::packages::PackageRegistry;
use crate::registry::{download_from_url, RegistryClient};
use crate::store::{
    list_all_bundles, repair_codegraph_view, resolve_bundle, BundleScope, BundleStore,
};

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
        // Refresh — rebuild current local workspace dependency
        // -----------------------------------------------------------------------
        Commands::Refresh => run_refresh(workspace),

        // -----------------------------------------------------------------------
        // List
        // -----------------------------------------------------------------------
        Commands::List => {
            let registry = PackageRegistry::load()?;
            let local_pkgs = registry.list();
            let bundles = list_all_bundles(workspace);
            // Workspace manifest carries any user-set bundle descriptions.
            let manifest = workspace.and_then(|ws| manifest::load_manifest(ws).ok());

            if local_pkgs.is_empty() && bundles.is_empty() {
                println!("No packages or bundles registered.");
                println!("  Local packages: sempkg pkg add <name> <path>");
                println!("  Bundles:        sempkg sync  or  sempkg install <name>@<version> --registry <url>");
                return Ok(());
            }

            if !local_pkgs.is_empty() {
                println!("Local packages:");
                for pkg in &local_pkgs {
                    let idx = if pkg.is_indexed() {
                        "indexed"
                    } else {
                        "NOT indexed"
                    };
                    let desc = if pkg.description.is_empty() {
                        String::new()
                    } else {
                        format!("  # {}", pkg.description)
                    };
                    println!("  {:<24} [{idx}]  {}{desc}", pkg.name, pkg.path);
                }
            }

            if !bundles.is_empty() {
                if !local_pkgs.is_empty() {
                    println!();
                }
                println!("Installed bundles:");
                for b in &bundles {
                    let idx = if b.is_indexed() {
                        "indexed"
                    } else {
                        "no graph"
                    };
                    let lance_flag = if b.has_lance() { "  +lance" } else { "" };
                    let code_flag = if b.has_code() { "  +code" } else { "" };
                    let scope = match b.scope {
                        BundleScope::Workspace => "workspace",
                        BundleScope::Global => "global",
                    };
                    let desc = manifest
                        .as_ref()
                        .and_then(|mf| mf.find_dependency(&b.name))
                        .and_then(|d| d.description.as_deref())
                        .filter(|d| !d.is_empty())
                        .map(|d| format!("  # {d}"))
                        .unwrap_or_default();
                    println!(
                        "  {:<20} @ {:<12} [{idx}]  [{scope}]{lance_flag}{code_flag}{desc}",
                        b.name, b.version
                    );
                }
            }

            Ok(())
        }

        // -----------------------------------------------------------------------
        // Add dependency to manifest
        // -----------------------------------------------------------------------
        Commands::Add {
            spec,
            registry_url,
            registry,
            group,
            url,
            build,
            reinstall,
            full,
            name: name_override,
            version: version_override,
            include_source,
            source_glob,
            source_dirs,
            docs_dirs,
            exclude_dirs,
            description,
        } => {
            let dir = require_workspace(workspace)?;

            let source_input = spec.as_deref().or_else(|| {
                url.as_deref()
                    .filter(|candidate| github::parse_source(candidate).is_some())
            });

            if let Some(source_input) = source_input {
                // Check if this is a local folder path first.
                if let Some(local_path) = parse_local_source(source_input) {
                    return add_from_local(
                        local_path,
                        dir,
                        group.as_deref(),
                        reinstall,
                        name_override.as_deref(),
                        version_override.as_deref(),
                        include_source,
                        source_glob.clone(),
                        source_dirs,
                        docs_dirs,
                        exclude_dirs,
                        description,
                        workspace,
                        None,
                    );
                }

                // Check if this is a GitHub source.
                if let Some(gh_src) = github::parse_source(source_input) {
                    return add_from_github(
                        gh_src,
                        dir,
                        group.as_deref(),
                        build,
                        reinstall,
                        full,
                        name_override.as_deref(),
                        version_override.as_deref(),
                        include_source,
                        source_glob.clone(),
                        source_dirs,
                        docs_dirs,
                        exclude_dirs,
                        description,
                        workspace,
                        None,
                    );
                }
            }

            // --- Existing registry / URL path (unchanged) ---
            let spec = spec.as_deref().ok_or_else(|| {
                anyhow::anyhow!(
                    "Direct bundle URLs still require <SPEC> in name@version format. \
                     Use `sempkg add <name>@<version> --url <bundle-asset-url>` for assets, \
                     or pass a GitHub source URL to `--url` to build from source."
                )
            })?;
            let (name, version) = parse_spec(&spec)?;

            let mut mf = manifest::load_manifest(dir)?;

            if let Some(direct_url) = url {
                // Direct URL dependency — no registry needed
                let dep = DependencyEntry {
                    version: version.to_string(),
                    registry: None,
                    url: Some(direct_url),
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
                    description: description.clone(),
                };
                insert_dep(&mut mf, name, dep, group.as_deref());
            } else if let Some(url) = &registry_url {
                // Add or update registry if a URL was provided
                let url = url.trim_end_matches('/');
                let reg_name = registry.clone().unwrap_or_else(|| "default".to_string());
                if mf.get_registry(&reg_name).is_none() {
                    mf.registries.push(RegistryEntry {
                        name: reg_name.clone(),
                        url: url.to_string(),
                    });
                }
                let dep = DependencyEntry {
                    version: version.to_string(),
                    registry: Some(reg_name),
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
                    description: description.clone(),
                };
                insert_dep(&mut mf, name, dep, group.as_deref());
            } else {
                if mf.registries.is_empty() {
                    anyhow::bail!(
                        "No registries defined in sempkg.toml. \
                         Use --registry-url to add one: sempkg add --registry-url <url> {spec}\n\
                         Or use --url to add a direct download URL: sempkg add --url <url> {spec}\n\
                         Or install directly from GitHub: sempkg add owner/repo@ref"
                    );
                }
                let resolved_registry =
                    registry.or_else(|| mf.default_registry().map(|r| r.name.clone()));
                let dep = DependencyEntry {
                    version: version.to_string(),
                    registry: resolved_registry,
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
                    description: description.clone(),
                };
                insert_dep(&mut mf, name, dep, group.as_deref());
            }

            manifest::save_manifest(&mf, dir)?;
            if let Some(g) = &group {
                println!("Added {name}@{version} to group '{g}' in sempkg.toml. Run 'sempkg sync --group {g}' to install.");
            } else {
                println!("Added {name}@{version} to sempkg.toml. Run 'sempkg sync' to install.");
            }
            Ok(())
        }

        // -----------------------------------------------------------------------
        // Remove dependency from manifest and/or uninstall from store
        // -----------------------------------------------------------------------
        Commands::Remove {
            name,
            group,
            global,
        } => {
            if global {
                if group.is_some() {
                    anyhow::bail!("--group cannot be used with --global");
                }

                let mut removed_any = false;

                let mut registry = PackageRegistry::load()?;
                if registry.remove(&name)? {
                    println!("Removed '{name}' from the global package registry.");
                    removed_any = true;
                }

                let removed_versions = BundleStore::global().remove_package(&name)?;
                if removed_versions > 0 {
                    println!(
                        "Removed {name} from the global bundle store ({removed_versions} version(s))."
                    );
                    removed_any = true;
                }

                if !removed_any {
                    anyhow::bail!(
                        "'{name}' not found in the global package registry or global bundle store."
                    );
                }

                return Ok(());
            }

            let dir = require_workspace(workspace)?;
            let mut mf = manifest::load_manifest(dir)?;

            let removed = if let Some(g) = &group {
                mf.dependency_groups
                    .get_mut(g)
                    .and_then(|grp| grp.remove(&name))
                    .is_some()
            } else {
                mf.dependencies.remove(&name).is_some()
            };

            if !removed {
                let hint = if group.is_none() {
                    " Use --group <name> to remove from a specific group.".to_string()
                } else {
                    String::new()
                };
                anyhow::bail!("'{name}' not found in sempkg.toml.{hint}");
            }

            manifest::save_manifest(&mf, dir)?;
            if let Some(g) = &group {
                println!("Removed '{name}' from group '{g}' in sempkg.toml.");
            } else {
                println!("Removed '{name}' from sempkg.toml.");
            }

            let removed_versions = BundleStore::workspace(dir).remove_package(&name)?;
            if removed_versions > 0 {
                println!(
                    "Removed {name} from the workspace store ({removed_versions} version(s))."
                );
            }

            Ok(())
        }

        // -----------------------------------------------------------------------
        // Sync — install all workspace manifest dependencies
        // -----------------------------------------------------------------------
        Commands::Sync {
            reinstall,
            group,
            all_groups,
        } => {
            let dir = require_workspace(workspace)?;
            let mf = manifest::load_manifest(dir)?;
            let store = BundleStore::workspace(dir);
            let verify_key_path = mf.verify_key_path(dir);

            let deps = mf.resolve_deps(&group, all_groups);

            if deps.is_empty() {
                println!("No dependencies in sempkg.toml.");
                return Ok(());
            }

            if !group.is_empty() {
                println!("Installing [dependencies] + groups: {}", group.join(", "));
            } else if all_groups {
                println!("Installing [dependencies] + all dependency groups");
            }

            let verify_key = verify_key_path
                .as_deref()
                .map(verify::load_verifying_key)
                .transpose()?;

            let mut lock = manifest::load_lock(dir)?;
            let mut installed = Vec::new();

            for (dep_name, dep) in &deps {
                // GitHub-sourced dependency — re-run the build flow
                if dep.git.is_some() {
                    let git_src = dep.git.as_deref().unwrap_or_default();
                    let (host, owner, repo) =
                        parse_manifest_git_source(git_src).ok_or_else(|| {
                            anyhow::anyhow!(
                                "Invalid git source '{git_src}' for dependency '{dep_name}'. \
                             Expected github:owner/repo or github:host/owner/repo"
                            )
                        })?;

                    let gh_src = github::GitHubSource {
                        host,
                        owner,
                        repo,
                        git_ref: dep.git_ref.clone().or_else(|| Some(dep.version.clone())),
                        subdir: dep.subdir.clone(),
                    };
                    let full = dep.full;
                    println!(
                        "  Syncing {dep_name} from {} ...",
                        dep.git.as_deref().unwrap_or("github")
                    );
                    add_from_github(
                        gh_src,
                        dir,
                        None,
                        false,
                        reinstall,
                        full,
                        Some(dep_name),
                        None,
                        dep.include_source,
                        dep.source_glob.clone(),
                        dep.source_dirs.iter().map(PathBuf::from).collect(),
                        dep.docs_dirs.iter().map(PathBuf::from).collect(),
                        dep.exclude_dirs.iter().map(PathBuf::from).collect(),
                        dep.description.clone(),
                        workspace,
                        Some(&mut lock),
                    )?;
                    installed.push(format!("{dep_name}@{}", dep.version));
                    continue;
                }

                // Local folder dependency — re-build from the stored path
                if let Some(local_path_str) = &dep.local {
                    // The stored path may be relative to the workspace dir.
                    let raw = PathBuf::from(local_path_str);
                    let local_path = if raw.is_absolute() {
                        raw
                    } else {
                        dir.join(raw)
                    };
                    println!(
                        "  Syncing {dep_name} from local path {} ...",
                        local_path.display()
                    );
                    add_from_local(
                        local_path,
                        dir,
                        None,
                        reinstall,
                        Some(dep_name),
                        Some(&dep.version),
                        dep.include_source,
                        dep.source_glob.clone(),
                        dep.source_dirs.iter().map(PathBuf::from).collect(),
                        dep.docs_dirs.iter().map(PathBuf::from).collect(),
                        dep.exclude_dirs.iter().map(PathBuf::from).collect(),
                        dep.description.clone(),
                        workspace,
                        Some(&mut lock),
                    )?;
                    installed.push(format!("{dep_name}@{}", dep.version));
                    continue;
                }

                let source_label = if let Some(direct_url) = &dep.url {
                    direct_url.clone()
                } else {
                    mf.registry_for(dep)
                        .with_context(|| format!("No registry found for dependency '{dep_name}'"))?
                        .url
                        .clone()
                };

                if !reinstall && store.is_installed(dep_name, &dep.version) {
                    println!("  {dep_name}@{} already installed, skipping.", dep.version);
                    record_installed_bundle_lock(
                        dir,
                        Some(&mut lock),
                        dep_name,
                        &dep.version,
                        source_label,
                        dep.url.is_none() && verify_key.is_some(),
                        None,
                    )?;
                    continue;
                }

                let bytes: Vec<u8>;

                if let Some(direct_url) = &dep.url {
                    // Direct URL dependency (e.g. GitHub release asset)
                    print!("  Installing {dep_name}@{} from URL ... ", dep.version);
                    std::io::Write::flush(&mut std::io::stdout())?;
                    bytes = download_from_url(direct_url, None)?;
                } else {
                    let reg = mf.registry_for(dep).with_context(|| {
                        format!("No registry found for dependency '{dep_name}'")
                    })?;

                    print!(
                        "  Installing {dep_name}@{} from {} ... ",
                        dep.version, reg.name
                    );
                    std::io::Write::flush(&mut std::io::stdout())?;

                    let client = RegistryClient::new(&reg.url);
                    let index_entry = client.lookup(dep_name, &dep.version).ok();
                    let expected_sha256 = index_entry.as_ref().and_then(|e| e.sha256.as_deref());
                    bytes = client.download_bundle(dep_name, &dep.version, expected_sha256)?;

                    // Signature verification
                    if let Some(key) = &verify_key {
                        let sig = client.download_signature(dep_name, &dep.version)?;
                        verify::verify_bundle_signature(&bytes, &sig, key).with_context(|| {
                            format!(
                                "Signature verification failed for {dep_name}@{}",
                                dep.version
                            )
                        })?;
                    }
                }

                store.install_bytes(&bytes)?;
                println!("done.");

                record_installed_bundle_lock(
                    dir,
                    Some(&mut lock),
                    dep_name,
                    &dep.version,
                    source_label,
                    dep.url.is_none() && verify_key.is_some(),
                    None,
                )?;
                installed.push(format!("{dep_name}@{}", dep.version));
            }

            let valid_names: std::collections::BTreeSet<String> =
                mf.resolve_deps(&[], true).keys().cloned().collect();
            let installed_names: std::collections::BTreeSet<String> =
                store.list().into_iter().map(|bundle| bundle.name).collect();
            lock.packages.retain(|entry| {
                valid_names.contains(&entry.name) || installed_names.contains(&entry.name)
            });
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
        Commands::Install {
            spec,
            global,
            registry: reg_url,
            verify_key,
            url,
        } => {
            let (name, version) = parse_spec(&spec)?;

            let store = if global {
                BundleStore::global()
            } else {
                BundleStore::workspace(require_workspace(workspace)?)
            };

            let scope_label = if global { "global" } else { "workspace" };

            let bytes = if let Some(direct_url) = &url {
                print!("Downloading {name}@{version} from URL ... ");
                std::io::Write::flush(&mut std::io::stdout())?;
                let b = download_from_url(direct_url, None)?;
                println!("done.");
                b
            } else {
                let reg_url = reg_url
                    .as_deref()
                    .expect("--registry required when --url is not provided");
                let client = RegistryClient::new(reg_url);

                let index_entry = client.lookup(name, version).ok();
                let expected_sha256 = index_entry.as_ref().and_then(|e| e.sha256.as_deref());

                print!("Downloading {name}@{version} ... ");
                std::io::Write::flush(&mut std::io::stdout())?;
                let b = client.download_bundle(name, version, expected_sha256)?;
                println!("done.");
                b
            };

            // Signature verification (registry-sourced only, URLs don't carry .sig)
            if let Some(key_path) = &verify_key {
                if url.is_some() {
                    eprintln!(
                        "Warning: signature verification is not supported for direct URL installs."
                    );
                } else {
                    let reg_url = reg_url.as_deref().unwrap();
                    let client = RegistryClient::new(reg_url);
                    let key = verify::load_verifying_key(key_path)?;
                    let sig = client.download_signature(name, version)?;
                    verify::verify_bundle_signature(&bytes, &sig, &key)
                        .context("Signature verification failed")?;
                    println!("Signature verified.");
                }
            }

            let info = store.install_bytes(&bytes)?;
            println!(
                "Installed {}@{} [{scope_label}]{}{}",
                info.name,
                info.version,
                if info.has_lance() { "  +lance" } else { "" },
                if info.has_code() { "  +code" } else { "" },
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
                    match graph::status_text(&pkg.abs_path()) {
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
                println!(
                    "  .codegraph: {}",
                    bundle.bundle_dir.join(".codegraph").exists()
                );
                println!("  Queryable:  {}", bundle.is_indexed());
                println!("  Lance:      {}", bundle.has_lance());
                println!("  Code index: {}", bundle.has_code());
                println!("  Source:     {}", bundle.manifest.source_repo);
                println!("  Commit:     {}", bundle.manifest.commit_hash);
                println!("  Created:    {}", bundle.manifest.created_at);
                if bundle.bundle_dir.join("graph").exists()
                    && !bundle.bundle_dir.join(".codegraph").exists()
                {
                    println!("\n  ! .codegraph view missing — run 'sempkg repair' to fix.");
                }
                return Ok(());
            }

            anyhow::bail!("'{name}' not found. Run 'sempkg list' to see available packages.");
        }

        // -----------------------------------------------------------------------
        // Uninstall — remove a bundle from store
        // -----------------------------------------------------------------------
        Commands::Uninstall { spec, global } => {
            let (name, version) = parse_spec(&spec)?;

            let store = if global {
                BundleStore::global()
            } else {
                BundleStore::workspace(require_workspace(workspace)?)
            };

            let scope_label = if global { "global" } else { "workspace" };

            store.remove(name, version)?;
            println!("Uninstalled {name}@{version} [{scope_label}].");
            Ok(())
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
                    Ok(false) => {
                        already_ok += 1;
                    }
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
        Commands::Search {
            package,
            query,
            kind,
            limit,
        } => {
            let path = resolve_codegraph_path(&package, workspace)?;
            println!("{}", graph::query(&path, &query, kind.as_deref(), limit)?);
            Ok(())
        }

        Commands::Callers {
            package,
            symbol,
            limit,
        } => {
            let path = resolve_codegraph_path(&package, workspace)?;
            println!("{}", graph::callers(&path, &symbol, limit)?);
            Ok(())
        }

        Commands::Callees {
            package,
            symbol,
            limit,
        } => {
            let path = resolve_codegraph_path(&package, workspace)?;
            println!("{}", graph::callees(&path, &symbol, limit)?);
            Ok(())
        }

        Commands::Context { package, task } => {
            let path = resolve_codegraph_path(&package, workspace)?;
            // Default node budget for the CLI surface (the MCP tool passes its
            // own reranker-derived budget).
            const CLI_CONTEXT_MAX_NODES: usize = 20;
            println!("{}", graph::context(&path, &task, CLI_CONTEXT_MAX_NODES)?);
            Ok(())
        }

        Commands::Impact {
            package,
            symbol,
            depth,
        } => {
            let path = resolve_codegraph_path(&package, workspace)?;
            println!("{}", graph::impact(&path, &symbol, depth)?);
            Ok(())
        }

        Commands::Files {
            package,
            filter,
            limit,
        } => {
            let path = resolve_codegraph_path(&package, workspace)?;
            println!("{}", graph::files(&path, filter.as_deref(), limit)?);
            Ok(())
        }

        // -----------------------------------------------------------------------
        // LanceDB doc search
        // -----------------------------------------------------------------------
        Commands::Docs {
            package,
            query,
            limit,
        } => {
            let bundle_dir = resolve_lance_path(&package, workspace)?;
            let results = lance::search(&bundle_dir, &query, limit)?;
            println!("{}", lance::format_results(&results, &query));
            Ok(())
        }

        // -----------------------------------------------------------------------
        // Hybrid search with reranking
        // -----------------------------------------------------------------------
        Commands::Query {
            package,
            query,
            docs,
            code,
            kind,
            limit,
            top_k,
        } => run_query(
            &package,
            &query,
            docs,
            code,
            kind.as_deref(),
            limit,
            top_k,
            workspace,
        ),

        Commands::DocsMeta { package } => {
            let bundle_dir = resolve_lance_path(&package, workspace)?;
            if let Some(meta) = lance::load_metadata(&bundle_dir) {
                println!("LanceDB metadata for '{package}':");
                println!(
                    "  Table:       {}",
                    meta.table_name.as_deref().unwrap_or("docs")
                );
                println!("  Documents:   {}", meta.document_count.unwrap_or(0));
                println!("  Chunks:      {}", meta.chunk_count.unwrap_or(0));
                println!("  FTS enabled: {}", meta.fts_enabled.unwrap_or(false));
                println!(
                    "  Indexed at:  {}",
                    meta.created_at.as_deref().unwrap_or("unknown")
                );
            } else {
                anyhow::bail!("No LanceDB metadata found for '{package}'.");
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

        // -----------------------------------------------------------------------
        // Reranker management
        // -----------------------------------------------------------------------
        Commands::Reranker(sub) => run_reranker(sub, workspace),

        // -----------------------------------------------------------------------
        // Embedding / query-expansion management
        // -----------------------------------------------------------------------
        Commands::Embed { package, force } => run_embed(package.as_deref(), force, workspace),
        Commands::Embedding(sub) => run_embedding(sub, workspace),
        Commands::QueryExpansion(sub) => run_query_expansion(sub, workspace),
    }
}

// ---------------------------------------------------------------------------
// query — hybrid symbol + doc search with reranking
// ---------------------------------------------------------------------------

/// Run `sempkg query`: fetch from both CodeGraph and LanceDB, then rerank.
///
/// Corresponds to QMD's "query" level (BM25 + re-ranking); the existing
/// `search` and `docs` commands remain the fast BM25-only paths.
fn run_query(
    package: &str,
    query: &str,
    docs_only: bool,
    code_only: bool,
    kind: Option<&str>,
    limit: usize,
    top_k_override: Option<usize>,
    workspace: Option<&Path>,
) -> Result<()> {
    let mut cfg: reranker::RerankerConfig = workspace
        .and_then(|d| manifest::load_manifest(d).ok())
        .and_then(|mf| mf.reranker)
        .unwrap_or_default();

    if let Some(k) = top_k_override {
        cfg.top_k = k;
    }

    let ranker = providers::build_reranker(&cfg).ok_or_else(|| {
        anyhow::anyhow!(
            "Reranker not available. \
             For local models: run `sempkg reranker pull` or build with `--features reranker`. \
             For remote: set `provider = \"openai\"` and configure `[reranker.openai]` in sempkg.toml."
        )
    })?;

    let fetch_k = ranker.top_k();

    let mut code_candidates: Vec<reranker::RerankCandidate> = Vec::new();
    if !docs_only {
        match resolve_codegraph_path(package, workspace) {
            Ok(path) => match graph::query(&path, query, kind, fetch_k) {
                Ok(raw) => code_candidates.extend(reranker::codegraph_json_to_candidates(&raw)),
                Err(e) => eprintln!("Warning: symbol search failed: {e}"),
            },
            Err(_) => {} // package may be docs-only; not fatal
        }
    }

    let mut doc_candidates: Vec<reranker::RerankCandidate> = Vec::new();
    if !code_only {
        if docs_only && kind.is_some() {
            eprintln!("Note: --kind is ignored when --docs is set.");
        }
        match resolve_lance_path(package, workspace) {
            Ok(lance_dir) => match lance::search(&lance_dir, query, fetch_k) {
                Ok(results) => {
                    doc_candidates.extend(reranker::lance_results_to_candidates(&results))
                }
                Err(e) => eprintln!("Warning: doc search failed: {e}"),
            },
            Err(_) => {} // package may be symbols-only; not fatal
        }
    }

    // Interleave sources so one side can't monopolize the pool.
    let candidates = merge_query_candidates(code_candidates, doc_candidates, fetch_k);

    if candidates.is_empty() {
        println!("No results for '{query}'.");
        return Ok(());
    }

    let has_codegraph = candidates
        .iter()
        .any(|c| c.origin == reranker::RerankOrigin::Codegraph);
    let has_docs = candidates
        .iter()
        .any(|c| c.origin == reranker::RerankOrigin::Docs);

    eprintln!("Scoring {} candidates...", candidates.len());
    // Score the full pool (override top_k/output_n on the fly).
    let all_scored: Vec<reranker::RerankResult> = {
        let pool: Vec<reranker::RerankCandidate> = candidates;
        let mut scored: Vec<reranker::RerankResult> = pool
            .into_iter()
            .map(|c| {
                let score = ranker.score_pair(query, &c.text).unwrap_or(0.0);
                reranker::RerankResult {
                    source: c.source,
                    text: c.text,
                    origin: c.origin,
                    score,
                }
            })
            .collect();
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        scored
    };
    let scored = diversify_query_results(all_scored, limit, has_codegraph, has_docs);

    if scored.is_empty() {
        println!("No results for '{query}'.");
    } else {
        println!("{}", reranker::format_reranked_docs(&scored, query));
    }
    Ok(())
}

/// Merge code and doc candidates into a single pool with a strict total budget.
///
/// Uses simple alternating interleave (code, doc, code, doc, ...) to keep both
/// sources represented when both exist. Preserves per-source ranking order.
fn merge_query_candidates(
    mut code: Vec<reranker::RerankCandidate>,
    mut docs: Vec<reranker::RerankCandidate>,
    total_k: usize,
) -> Vec<reranker::RerankCandidate> {
    if total_k == 0 {
        return Vec::new();
    }

    if code.is_empty() {
        docs.truncate(total_k);
        return docs;
    }
    if docs.is_empty() {
        code.truncate(total_k);
        return code;
    }

    let mut merged = Vec::with_capacity(total_k);
    let mut code_iter = code.into_iter();
    let mut docs_iter = docs.into_iter();

    loop {
        if merged.len() >= total_k {
            break;
        }

        if let Some(c) = code_iter.next() {
            merged.push(c);
        }
        if merged.len() >= total_k {
            break;
        }

        if let Some(d) = docs_iter.next() {
            merged.push(d);
        }

        // Stop when both sides are exhausted.
        if code_iter.len() == 0 && docs_iter.len() == 0 {
            break;
        }
    }

    merged
}

/// Keep hybrid query results source-diverse so docs do not disappear when the
/// reranker strongly prefers short symbol signatures for terse queries.
fn diversify_query_results(
    scored: Vec<reranker::RerankResult>,
    limit: usize,
    has_codegraph: bool,
    has_docs: bool,
) -> Vec<reranker::RerankResult> {
    if !has_codegraph || !has_docs || limit <= 1 {
        return scored.into_iter().take(limit).collect();
    }

    let mut final_results = Vec::new();
    let mut best_doc: Option<reranker::RerankResult> = None;
    let mut best_code: Option<reranker::RerankResult> = None;

    for result in &scored {
        match result.origin {
            reranker::RerankOrigin::Docs if best_doc.is_none() => {
                best_doc = Some(result.clone());
            }
            reranker::RerankOrigin::Codegraph if best_code.is_none() => {
                best_code = Some(result.clone());
            }
            _ => {}
        }
        if best_doc.is_some() && best_code.is_some() {
            break;
        }
    }

    if let Some(code) = best_code {
        final_results.push(code);
    }
    if let Some(doc) = best_doc {
        if !final_results
            .iter()
            .any(|r| r.source == doc.source && r.text == doc.text)
        {
            final_results.push(doc);
        }
    }

    for result in scored {
        if final_results.len() >= limit {
            break;
        }
        if final_results
            .iter()
            .any(|r| r.source == result.source && r.text == result.text)
        {
            continue;
        }
        final_results.push(result);
    }

    final_results.truncate(limit);
    final_results
}

// ---------------------------------------------------------------------------
// reranker sub-commands
// ---------------------------------------------------------------------------

fn run_reranker(cmd: RerankerCommands, workspace: Option<&Path>) -> Result<()> {
    // Load config from workspace manifest (or use defaults).
    let cfg: reranker::RerankerConfig = workspace
        .and_then(|d| manifest::load_manifest(d).ok())
        .and_then(|mf| mf.reranker)
        .unwrap_or_default();

    match cmd {
        RerankerCommands::Pull { gguf_url, hf_token } => {
            if cfg.provider != providers::ProviderKind::Local {
                anyhow::bail!(
                    "`sempkg reranker pull` only applies to `provider = \"local\"`. \
                     Remote providers (openai, copilot) do not require model downloads."
                );
            }
            let pull_cfg = cfg.clone();
            let token = hf_token.as_deref();
            let source_url = gguf_url.as_deref();

            println!("Pulling Qwen3-Reranker-0.6B GGUF model...");
            reranker::pull_model(&pull_cfg, token, source_url)?;

            println!();
            println!(
                "Model ready. Reranking works without a [reranker] table (defaults apply).\n\
                 Add this optional section only if you want workspace defaults:\n\n\
                 [reranker]\n\
                 enabled  = true\n\
                 top_k    = 20\n\
                 output_n = 5\n"
            );
            Ok(())
        }

        RerankerCommands::Status => {
            reranker::print_status(&cfg);
            Ok(())
        }

        RerankerCommands::Test { query, document } => {
            println!("Loading reranker...");
            let ranker = providers::build_reranker(&cfg).ok_or_else(|| {
                anyhow::anyhow!(
                    "Reranker not available. \
                     For local models: run `sempkg reranker pull` or build with `--features reranker`. \
                     For remote: set `provider = \"openai\"` in [reranker] config."
                )
            })?;
            let candidates = vec![reranker::RerankCandidate {
                source: "test-document".to_string(),
                text: document.clone(),
                origin: reranker::RerankOrigin::Docs,
            }];
            let results = ranker.rerank(&query, candidates)?;
            if let Some(r) = results.first() {
                println!(
                    "Score: {:.4}  (1.0 = highly relevant, 0.0 = not relevant)",
                    r.score
                );
            } else {
                println!("No results.");
            }
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// embedding management
// ---------------------------------------------------------------------------

/// Load the embedding config from the workspace manifest, or defaults.
fn load_embedding_cfg(workspace: Option<&Path>) -> embedding::EmbeddingConfig {
    workspace
        .and_then(|d| manifest::load_manifest(d).ok())
        .and_then(|mf| mf.embedding)
        .unwrap_or_default()
}

/// Load the query-expansion config from the workspace manifest, or defaults.
fn load_query_expansion_cfg(workspace: Option<&Path>) -> query_expansion::QueryExpansionConfig {
    workspace
        .and_then(|d| manifest::load_manifest(d).ok())
        .and_then(|mf| mf.query_expansion)
        .unwrap_or_default()
}

fn run_embedding(cmd: EmbeddingCommands, workspace: Option<&Path>) -> Result<()> {
    let cfg = load_embedding_cfg(workspace);

    match cmd {
        EmbeddingCommands::Pull {
            model,
            gguf_url,
            hf_token,
        } => {
            if cfg.provider != providers::ProviderKind::Local {
                anyhow::bail!(
                    "`sempkg embedding pull` only applies to `provider = \"local\"`. \
                     Remote providers (openai, copilot) do not require model downloads."
                );
            }
            // `--model` selects a specific model (downloaded to its default
            // path); otherwise pull whatever the workspace is configured to use.
            let (selected, dest) = match model {
                Some(id) => {
                    let m = embedding::EmbeddingModel::from_id(&id).ok_or_else(|| {
                        anyhow::anyhow!(
                            "unknown embedding model '{id}'. Known models: {}",
                            embedding::EmbeddingModel::KNOWN_IDS.join(", ")
                        )
                    })?;
                    (m, embedding::default_model_dir().join(m.default_filename()))
                }
                None => (cfg.model()?, cfg.resolved_model_path()),
            };

            println!("Pulling {} GGUF model...", selected.display_name());
            embedding::pull_model(selected, &dest, hf_token.as_deref(), gguf_url.as_deref())?;
            println!();

            // If the pulled model differs from the configured one, tell the user
            // how to activate it.
            let configured = cfg.model().unwrap_or(embedding::EmbeddingModel::DEFAULT);
            if selected != configured {
                println!(
                    "To use {}, set `model_id = \"{}\"` under [embedding] in sempkg.toml,",
                    selected.display_name(),
                    selected.id()
                );
                println!("then run `sempkg embed --force` to re-embed your bundles.");
            } else {
                println!(
                    "Model ready. Run `sempkg embed` to build vector indexes for your bundles."
                );
            }
            Ok(())
        }
        EmbeddingCommands::Status => {
            embedding::print_status(&cfg);
            Ok(())
        }
    }
}

fn run_query_expansion(cmd: QueryExpansionCommands, workspace: Option<&Path>) -> Result<()> {
    let cfg = load_query_expansion_cfg(workspace);

    match cmd {
        QueryExpansionCommands::Pull { gguf_url, hf_token } => {
            if cfg.provider != providers::ProviderKind::Local {
                anyhow::bail!(
                    "`sempkg query-expansion pull` only applies to `provider = \"local\"`. \
                     Remote providers (openai, copilot) do not require model downloads."
                );
            }
            println!("Pulling query-expansion GGUF model...");
            query_expansion::pull_model(&cfg, hf_token.as_deref(), gguf_url.as_deref())?;
            println!();
            println!("Model ready. The MCP `query` tool will expand queries automatically.");
            Ok(())
        }
        QueryExpansionCommands::Status => {
            query_expansion::print_status(&cfg);
            Ok(())
        }
        QueryExpansionCommands::Test { query } => {
            println!("Loading query-expansion model...");
            let expander = providers::build_expander(&cfg).ok_or_else(|| {
                    anyhow::anyhow!(
                        "Query expander not available. \
                         For local models: run `sempkg query-expansion pull` or build with `--features embeddings`. \
                         For remote: set `provider = \"openai\"` in [query_expansion] config."
                    )
                })?;
            let variants = expander.expand(&query);
            if variants.is_empty() {
                println!("(no variants produced — falling back to the original query)");
            } else {
                println!("Original: {query}");
                for v in &variants {
                    let route = match v.kind {
                        query_expansion::ExpansionKind::Lexical => "lex",
                        query_expansion::ExpansionKind::Vector => "vec",
                    };
                    println!("  [{route}] {}", v.text);
                }
            }
            Ok(())
        }
    }
}

/// Run `sempkg embed`: generate vector embeddings for installed bundles and
/// local packages so the MCP `query` tool can run semantic search.
fn run_embed(package: Option<&str>, force: bool, workspace: Option<&Path>) -> Result<()> {
    let cfg = load_embedding_cfg(workspace);

    let embedder = providers::build_embedder(&cfg).ok_or_else(|| {
        anyhow::anyhow!(
            "Embedder not available. \
             For local models: run `sempkg embedding pull` or build with `--features embeddings`. \
             For remote: set `provider = \"openai\"` and configure `[embedding.openai]` in sempkg.toml."
        )
    })?;

    println!("Loading embedding model ({})...", embedder.model_id());

    // Collect (label, table_dir, table_name) targets.
    let mut targets: Vec<(String, PathBuf, &'static str)> = Vec::new();

    // Local registered packages (docs index only).
    let reg = PackageRegistry::load()?;
    for pkg in reg.list() {
        if let Some(name) = package {
            if pkg.name != name {
                continue;
            }
        }
        let lance_dir = pkg.abs_path().join(".sempkg").join("lance");
        if lance_dir.is_dir() {
            targets.push((format!("{} (docs)", pkg.name), lance_dir, "docs"));
        }
    }

    // Installed bundles (docs and/or code).
    for bundle in list_all_bundles(workspace) {
        if let Some(name) = package {
            if bundle.name != name && format!("{}@{}", bundle.name, bundle.version) != name {
                continue;
            }
        }
        if bundle.has_lance() {
            targets.push((
                format!("{}@{} (docs)", bundle.name, bundle.version),
                bundle.bundle_dir.join("lance"),
                "docs",
            ));
        }
        if bundle.has_code() {
            targets.push((
                format!("{}@{} (code)", bundle.name, bundle.version),
                bundle.bundle_dir.join("code"),
                "code",
            ));
        }
    }

    if targets.is_empty() {
        if let Some(name) = package {
            anyhow::bail!("No indexed docs/code tables found for '{name}'.");
        }
        println!("No indexed docs/code tables found to embed.");
        return Ok(());
    }

    let mut total_rows: u64 = 0;
    for (label, dir, table) in &targets {
        // Skip tables already embedded with this model unless --force.
        if !force {
            if let Some((existing_model, existing_dim)) = lance::read_embedding_info(dir) {
                if existing_model == embedder.model_id() && existing_dim as usize == embedder.dim()
                {
                    println!("  ✓ {label}: already embedded (skipping; use --force to redo)");
                    continue;
                }
            }
        }

        print!("  • {label}: embedding... ");
        use std::io::Write;
        let _ = std::io::stdout().flush();
        match lance::embed_table(dir, table, embedder.as_ref()) {
            Ok(n) => {
                total_rows += n;
                println!("{n} rows");
            }
            Err(e) => println!("failed: {e}"),
        }
    }

    println!();
    println!(
        "Done. Embedded {total_rows} rows across {} table(s).",
        targets.len()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// index — one-shot workspace indexing
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
                    let idx = if pkg.is_indexed() {
                        "indexed"
                    } else {
                        "NOT indexed"
                    };
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

        PkgCommands::Add {
            name,
            path,
            description,
        } => {
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
                        if !out.is_empty() {
                            println!("{out}");
                        }
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
            let pkg = reg
                .get(&name)
                .with_context(|| format!("Package '{name}' not found."))?;
            print!("Reindexing '{}' ... ", pkg.path);
            std::io::Write::flush(&mut std::io::stdout())?;
            let out = if pkg.is_indexed() {
                codegraph::sync(&pkg.abs_path())?
            } else {
                codegraph::init_and_index(&pkg.abs_path())?
            };
            println!("done.");
            if !out.is_empty() {
                println!("{out}");
            }
            Ok(())
        }

        PkgCommands::Status { name } => {
            let reg = PackageRegistry::load()?;
            let pkg = reg
                .get(&name)
                .with_context(|| format!("Package '{name}' not found."))?;
            println!("{}", graph::status_text(&pkg.abs_path())?);
            Ok(())
        }

        PkgCommands::LanceIndex { name, pattern } => {
            let reg = PackageRegistry::load()?;
            let pkg = reg
                .get(&name)
                .with_context(|| format!("Package '{name}' not found."))?;
            println!(
                "Building LanceDB index for '{name}' (pattern: {pattern})\n\
                 Index stored at: {}/.sempkg/lance/",
                pkg.path
            );
            let lance_dir = lance::cli_update(&pkg.abs_path(), &pkg.name, &pattern)?;
            println!("LanceDB index ready at {}", lance_dir.display());
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

// ---------------------------------------------------------------------------
// GitHub add-from-source orchestration
// ---------------------------------------------------------------------------

/// Fetch, build (or download a release asset), and install a bundle from a
/// GitHub source. Records the dependency in `sempkg.toml` and `sempkg.lock`.
#[allow(clippy::too_many_arguments)]
fn add_from_github(
    src: github::GitHubSource,
    workspace_dir: &Path,
    group: Option<&str>,
    force_build: bool,
    reinstall: bool,
    full_clone: bool,
    name_override: Option<&str>,
    version_override: Option<&str>,
    include_source: bool,
    source_glob: Option<String>,
    source_dirs_override: Vec<PathBuf>,
    docs_dirs_override: Vec<PathBuf>,
    exclude_dirs: Vec<PathBuf>,
    description: Option<String>,
    _workspace: Option<&Path>,
    lock: Option<&mut manifest::LockFile>,
) -> Result<()> {
    let token = github::github_token_for_host(&src.host);
    let token_ref = token.as_deref();

    // 1. Resolve the ref to a full commit SHA
    eprintln!("[sempkg] Resolving {}/{} ...", src.owner, src.repo);
    let mut resolved = github::resolve(&src, token_ref)?;

    // Allow name/version overrides
    if let Some(n) = name_override {
        resolved.package_name = n.to_owned();
    }
    if let Some(v) = version_override {
        resolved.version = v.to_owned();
    }

    let store = BundleStore::workspace(workspace_dir);

    // Check if already installed
    if !reinstall && store.is_installed(&resolved.package_name, &resolved.version) {
        println!(
            "{}@{} is already installed. Use --reinstall to rebuild.",
            resolved.package_name, resolved.version
        );
        // Still write to manifest/lock if not already there
        record_github_dep(
            workspace_dir,
            &resolved,
            &src,
            group,
            full_clone,
            include_source,
            source_glob,
            &source_dirs_override,
            &docs_dirs_override,
            &exclude_dirs,
            description,
            lock,
        )?;
        return Ok(());
    }

    // 2. Fast path: check for a release asset
    let bytes: Vec<u8>;
    let source_label: String;
    let sig_url: Option<String>;

    if !force_build {
        if let Some(asset) = github::find_release_bundle_asset(&resolved, token_ref)? {
            eprintln!(
                "[sempkg] Found .sembundle release asset for {}@{} — downloading ...",
                resolved.package_name, resolved.version
            );
            bytes = registry::download_from_url(&asset.bundle_url, None)?;
            source_label = asset.bundle_url.clone();
            sig_url = asset.sig_url;
            // Install fast path
            return install_github_bundle(
                bytes,
                sig_url.as_deref(),
                source_label,
                &resolved,
                &src,
                workspace_dir,
                group,
                store,
                false, // release asset — full_clone does not apply
                false, // release asset — source index not rebuilt
                None,
                vec![],
                vec![],
                vec![],
                description,
                lock,
            );
        }
    }

    // 3. Build path: download tarball or full clone, extract, build
    let tmp = tempfile::TempDir::new().context("Failed to create temp directory")?;

    let root = if full_clone {
        github::git_clone_at_ref(&resolved, tmp.path())?
    } else {
        let archive_url = github::archive_tarball_url(&resolved);
        github::download_and_extract_tarball(&archive_url, token_ref, tmp.path())?
    };

    // Apply subdir scoping if requested
    let source_root = match &src.subdir {
        Some(sub) => {
            let sub_path = root.join(sub);
            if !sub_path.is_dir() {
                anyhow::bail!("Subdir '{}' not found in the repository archive.", sub);
            }
            sub_path
        }
        None => root.clone(),
    };

    // Detect language
    let language = github::detect_language(&source_root);
    let cg_version = codegraph::version();

    eprintln!(
        "[sempkg] Building bundle for {}@{} (language: {language}, codegraph: {cg_version}) ...",
        resolved.package_name, resolved.version
    );

    // Build the bundle
    let bundle_output = tmp.path().join(format!(
        "{}-{}.sembundle",
        resolved.package_name, resolved.version
    ));

    let build_opts = sembundle::BuildOptions {
        name: resolved.package_name.clone(),
        version: resolved.version.clone(),
        source_repo: resolved.source_repo_url.clone(),
        commit_hash: resolved.commit_sha.clone(),
        tag: if resolved.is_tag {
            Some(resolved.git_ref.clone())
        } else {
            None
        },
        language,
        codegraph_version: cg_version,
        output_path: Some(bundle_output.clone()),
        source_dirs: if source_dirs_override.is_empty() {
            vec![source_root.clone()]
        } else {
            source_dirs_override
                .iter()
                .map(|d| {
                    if d.is_absolute() {
                        d.clone()
                    } else {
                        source_root.join(d)
                    }
                })
                .collect()
        },
        docs_dirs: if docs_dirs_override.is_empty() {
            vec![source_root.clone()]
        } else {
            docs_dirs_override
                .iter()
                .map(|d| {
                    if d.is_absolute() {
                        d.clone()
                    } else {
                        source_root.join(d)
                    }
                })
                .collect()
        },
        docs_glob: None,
        include_source,
        source_glob: source_glob.clone(),
        exclude_dirs: exclude_dirs
            .iter()
            .map(|d| {
                if d.is_absolute() {
                    d.clone()
                } else {
                    source_root.join(d)
                }
            })
            .collect(),
    };

    sembundle::build(build_opts).with_context(|| {
        format!(
            "Failed to build bundle for {}@{}. \
             Ensure `codegraph` is on your PATH.",
            resolved.package_name, resolved.version
        )
    })?;

    bytes = std::fs::read(&bundle_output)
        .with_context(|| format!("Cannot read built bundle at {}", bundle_output.display()))?;
    source_label = format!(
        "github:{}/{}/{}@{}",
        resolved.host, resolved.owner, resolved.repo, resolved.git_ref
    );

    install_github_bundle(
        bytes,
        None,
        source_label,
        &resolved,
        &src,
        workspace_dir,
        group,
        store,
        full_clone,
        include_source,
        source_glob,
        source_dirs_override,
        docs_dirs_override,
        exclude_dirs,
        description,
        lock,
    )
}

fn install_github_bundle(
    bytes: Vec<u8>,
    _sig_url: Option<&str>,
    source_label: String,
    resolved: &github::ResolvedSource,
    src: &github::GitHubSource,
    workspace_dir: &Path,
    group: Option<&str>,
    store: BundleStore,
    full_clone: bool,
    include_source: bool,
    source_glob: Option<String>,
    source_dirs_override: Vec<PathBuf>,
    docs_dirs_override: Vec<PathBuf>,
    exclude_dirs: Vec<PathBuf>,
    description: Option<String>,
    lock: Option<&mut manifest::LockFile>,
) -> Result<()> {
    // Remove existing bundle dir so install_bytes can extract the freshly-built one.
    let existing_dir = store.bundle_dir(&resolved.package_name, &resolved.version);
    if existing_dir.exists() {
        std::fs::remove_dir_all(&existing_dir).with_context(|| {
            format!(
                "Failed to remove existing bundle at {}",
                existing_dir.display()
            )
        })?;
    }

    let info = store.install_bytes(&bytes)?;

    println!(
        "Installed {}@{} from {}{}{}",
        info.name,
        info.version,
        source_label,
        if info.has_lance() { "  +lance" } else { "" },
        if info.has_code() { "  +code" } else { "" }
    );

    record_github_dep(
        workspace_dir,
        resolved,
        src,
        group,
        full_clone,
        include_source,
        source_glob,
        &source_dirs_override,
        &docs_dirs_override,
        &exclude_dirs,
        description,
        lock,
    )
}

fn record_github_dep(
    workspace_dir: &Path,
    resolved: &github::ResolvedSource,
    src: &github::GitHubSource,
    group: Option<&str>,
    full_clone: bool,
    include_source: bool,
    source_glob: Option<String>,
    source_dirs_override: &[PathBuf],
    docs_dirs_override: &[PathBuf],
    exclude_dirs: &[PathBuf],
    description: Option<String>,
    lock: Option<&mut manifest::LockFile>,
) -> Result<()> {
    let mut mf = manifest::load_manifest(workspace_dir)?;

    let dep = manifest::DependencyEntry {
        version: resolved.version.clone(),
        registry: None,
        url: None,
        git: Some(format_manifest_git_source(
            &resolved.host,
            &resolved.owner,
            &resolved.repo,
        )),
        git_ref: src.git_ref.clone(),
        subdir: src.subdir.clone(),
        full: full_clone,
        local: None,
        include_source,
        source_glob,
        source_dirs: source_dirs_override
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        docs_dirs: docs_dirs_override
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        exclude_dirs: exclude_dirs
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        description,
    };
    insert_dep(&mut mf, &resolved.package_name, dep, group);
    manifest::save_manifest(&mf, workspace_dir)?;

    record_installed_bundle_lock(
        workspace_dir,
        lock,
        &resolved.package_name,
        &resolved.version,
        format_manifest_git_source(&resolved.host, &resolved.owner, &resolved.repo),
        false,
        Some(resolved.commit_sha.clone()),
    )?;

    if let Some(g) = group {
        println!(
            "Recorded {}@{} in group '{}' in sempkg.toml.",
            resolved.package_name, resolved.version, g
        );
    } else {
        println!(
            "Recorded {}@{} in sempkg.toml.",
            resolved.package_name, resolved.version
        );
    }

    Ok(())
}

fn record_installed_bundle_lock(
    workspace_dir: &Path,
    lock: Option<&mut manifest::LockFile>,
    name: &str,
    version: &str,
    registry_url: String,
    signed: bool,
    commit_sha: Option<String>,
) -> Result<()> {
    let store = BundleStore::workspace(workspace_dir);
    let info = store.get_version(name, version).with_context(|| {
        format!("Installed bundle {name}@{version} is missing from the bundle store")
    })?;

    let entry = manifest::LockEntry {
        name: name.to_string(),
        version: version.to_string(),
        registry_url,
        sha256: info.archive_sha256,
        signed,
        manifest_checksums: info.manifest.checksums.clone(),
        commit_sha,
    };

    match lock {
        Some(lock) => {
            lock.upsert(entry);
            Ok(())
        }
        None => {
            let mut lock = manifest::load_lock(workspace_dir)?;
            lock.upsert(entry);
            manifest::save_lock(&lock, workspace_dir)
        }
    }
}

/// Insert a dependency into the manifest, either into [dependencies] or a named group.
///
/// When the incoming entry carries no description, any description already
/// recorded for this dependency is preserved. This keeps a user-set description
/// intact across `sempkg sync` / `refresh` (which rebuild the entry from source
/// metadata) and across a bare `sempkg add <same>` without `--description`.
fn insert_dep(
    mf: &mut manifest::WorkspaceManifest,
    name: &str,
    mut dep: manifest::DependencyEntry,
    group: Option<&str>,
) {
    if dep.description.is_none() {
        dep.description = mf.find_dependency(name).and_then(|d| d.description.clone());
    }
    if let Some(g) = group {
        mf.dependency_groups
            .entry(g.to_string())
            .or_default()
            .insert(name.to_string(), dep);
    } else {
        mf.dependencies.insert(name.to_string(), dep);
    }
}

/// Parse `name@version` spec.
fn parse_spec(spec: &str) -> Result<(&str, &str)> {
    spec.split_once('@')
        .ok_or_else(|| anyhow::anyhow!("Invalid spec '{spec}'. Expected format: name@version"))
}

/// Parse persisted git source notation from sempkg.toml.
///
/// Supported forms:
/// - `github:owner/repo` (legacy, implies github.com)
/// - `github:host/owner/repo` (enterprise-capable)
fn parse_manifest_git_source(git: &str) -> Option<(String, String, String)> {
    let raw = git.strip_prefix("github:")?;
    let parts: Vec<&str> = raw.split('/').collect();
    match parts.as_slice() {
        [owner, repo] => Some((
            "github.com".to_owned(),
            (*owner).to_owned(),
            (*repo).to_owned(),
        )),
        [host, owner, repo] => Some(((*host).to_owned(), (*owner).to_owned(), (*repo).to_owned())),
        _ => None,
    }
}

fn format_manifest_git_source(host: &str, owner: &str, repo: &str) -> String {
    if host.eq_ignore_ascii_case("github.com") {
        format!("github:{owner}/{repo}")
    } else {
        format!("github:{host}/{owner}/{repo}")
    }
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
            anyhow::bail!("Bundle '{name}@{}' has no codegraph index.", bundle.version);
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

/// Resolve a name to its LanceDB-queryable directory.
/// Checks local packages first, then installed bundles.
fn resolve_lance_path(name: &str, workspace: Option<&Path>) -> Result<PathBuf> {
    let reg = PackageRegistry::load()?;
    if let Some(pkg) = reg.get(name) {
        let lance_dir = pkg.abs_path().join(".sempkg").join("lance");
        if lance_dir.is_dir() {
            return Ok(lance_dir);
        }
        anyhow::bail!(
            "Package '{name}' has no LanceDB index. Run 'sempkg pkg lance-index {name}' to build one."
        );
    }

    if let Some(bundle) = resolve_bundle(name, workspace) {
        if !bundle.has_lance() {
            anyhow::bail!(
                "Bundle '{name}@{}' does not have a LanceDB documentation index.",
                bundle.version
            );
        }
        return Ok(bundle.bundle_dir.join("lance"));
    }

    anyhow::bail!("'{name}' not found. Run 'sempkg list' to see available packages and bundles.")
}

// ---------------------------------------------------------------------------
// Local folder source detection
// ---------------------------------------------------------------------------

/// Detect whether `spec` looks like a local filesystem path.
///
/// Accepted forms:
/// - Absolute Unix paths: `/usr/lib/llvm`
/// - Absolute Windows paths: `C:\LLVM` or `C:/LLVM`
/// - Relative paths starting with `./` or `../` (`.\` / `..\` on Windows)
/// - Home-relative paths starting with `~/` or `~\`
///
/// Returns `None` if the spec does not match any of these forms, allowing the
/// caller to fall through to the GitHub / registry resolution paths.
fn parse_local_source(spec: &str) -> Option<PathBuf> {
    let s = spec.trim();

    // Reject anything that looks like a URL or owner/repo shorthand early.
    if s.contains("://") || s.contains("github:") {
        return None;
    }

    let looks_local = s.starts_with('/')
        || s == "."
        || s == ".."
        || s.starts_with("./")
        || s.starts_with("../")
        || s.starts_with(".\\")
        || s.starts_with("..\\")
        || s.starts_with("~/")
        || s.starts_with("~\\")
        // Windows absolute path: drive letter + colon + separator
        || (s.len() >= 3
            && s.as_bytes()[1] == b':'
            && (s.as_bytes()[2] == b'\\' || s.as_bytes()[2] == b'/'));

    if !looks_local {
        return None;
    }

    // Expand `~` to the user home directory.
    let expanded: PathBuf =
        if let Some(rest) = s.strip_prefix("~/").or_else(|| s.strip_prefix("~\\")) {
            dirs::home_dir()?.join(rest)
        } else {
            PathBuf::from(s)
        };

    Some(expanded)
}

fn run_refresh(workspace: Option<&Path>) -> Result<()> {
    let dir = require_workspace(workspace)?;
    let canonical = dir
        .canonicalize()
        .with_context(|| format!("Failed to canonicalize workspace path '{}'", dir.display()))?;
    let manifest = manifest::load_manifest(dir)?;

    let (dep_name, dep, group_name) = find_local_dependency_for_workspace(&manifest, &canonical)?;
    let local_path = PathBuf::from(dep.local.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "Local dependency '{}' is missing its source path.",
            dep_name
        )
    })?);

    println!(
        "Refreshing {} from local path {} ...",
        dep_name,
        local_path.display()
    );

    add_from_local(
        local_path,
        dir,
        group_name.as_deref(),
        true,
        Some(dep_name.as_str()),
        Some(dep.version.as_str()),
        dep.include_source,
        dep.source_glob.clone(),
        dep.source_dirs.iter().map(PathBuf::from).collect(),
        dep.docs_dirs.iter().map(PathBuf::from).collect(),
        dep.exclude_dirs.iter().map(PathBuf::from).collect(),
        dep.description.clone(),
        workspace,
        None,
    )
}

fn find_local_dependency_for_workspace<'a>(
    manifest: &'a manifest::WorkspaceManifest,
    workspace_path: &Path,
) -> Result<(String, &'a DependencyEntry, Option<String>)> {
    let mut matches: Vec<(String, &'a DependencyEntry, Option<String>)> = Vec::new();

    for (name, dep) in &manifest.dependencies {
        if local_dep_matches_workspace(dep, workspace_path)? {
            matches.push((name.clone(), dep, None));
        }
    }

    for (group_name, deps) in &manifest.dependency_groups {
        for (name, dep) in deps {
            if local_dep_matches_workspace(dep, workspace_path)? {
                matches.push((name.clone(), dep, Some(group_name.clone())));
            }
        }
    }

    match matches.len() {
        0 => anyhow::bail!(
            "No local dependency in sempkg.toml points at '{}'. Add this workspace first with `sempkg add .`.",
            workspace_path.display()
        ),
        1 => Ok(matches.remove(0)),
        _ => {
            let names = matches
                .iter()
                .map(|(name, _, group)| match group {
                    Some(group_name) => format!("{name} (group {group_name})"),
                    None => name.clone(),
                })
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::bail!(
                "Multiple local dependencies point at '{}': {}. Remove duplicates or keep only one local workspace entry before running `sempkg refresh`.",
                workspace_path.display(),
                names
            )
        }
    }
}

fn local_dep_matches_workspace(dep: &DependencyEntry, workspace_path: &Path) -> Result<bool> {
    let Some(local_path) = dep.local.as_deref() else {
        return Ok(false);
    };

    // Stored path may be relative to the workspace directory.
    let raw = Path::new(local_path);
    let resolved = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        workspace_path.join(raw)
    };

    let canonical = resolved.canonicalize().with_context(|| {
        format!(
            "Failed to canonicalize local dependency path '{}' recorded in sempkg.toml",
            local_path
        )
    })?;

    Ok(canonical == workspace_path)
}

// ---------------------------------------------------------------------------
// Local folder add-from-source orchestration
// ---------------------------------------------------------------------------

/// Build (or rebuild) and install a bundle from a local folder.
///
/// Steps:
///  1. Canonicalize the path and validate it is a directory.
///  2. Derive package name from the folder's basename (or use override).
///  3. Derive version: try `git describe` / `git rev-parse --short HEAD`,
///     fallback to `"local"`.
///  4. Skip if already installed and `reinstall` is false.
///  5. Build the `.sembundle` archive with `sembundle::build`.
///  6. Install into the workspace bundle store.
///  7. Record `{ local = "<path>", version = "..." }` in `sempkg.toml`.
#[allow(clippy::too_many_arguments)]
fn add_from_local(
    local_path: PathBuf,
    workspace_dir: &Path,
    group: Option<&str>,
    reinstall: bool,
    name_override: Option<&str>,
    version_override: Option<&str>,
    include_source: bool,
    source_glob: Option<String>,
    source_dirs_override: Vec<PathBuf>,
    docs_dirs_override: Vec<PathBuf>,
    exclude_dirs: Vec<PathBuf>,
    description: Option<String>,
    _workspace: Option<&Path>,
    lock: Option<&mut manifest::LockFile>,
) -> Result<()> {
    // --- 1. Validate path ---------------------------------------------------
    if !local_path.exists() {
        anyhow::bail!("Local path '{}' does not exist.", local_path.display());
    }
    if !local_path.is_dir() {
        anyhow::bail!("Local path '{}' is not a directory.", local_path.display());
    }
    let canonical = local_path
        .canonicalize()
        .with_context(|| format!("Failed to canonicalize '{}'", local_path.display()))?;

    // --- 2. Package name ----------------------------------------------------
    let package_name: String = if let Some(n) = name_override {
        n.to_string()
    } else {
        canonical
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            // Replace characters that are invalid in bundle names with `-`.
            .map(|n| {
                n.chars()
                    .map(|c| {
                        if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' {
                            c
                        } else {
                            '-'
                        }
                    })
                    .collect()
            })
            .unwrap_or_else(|| "local-package".to_string())
    };

    // --- 3. Version ---------------------------------------------------------
    let version: String = if let Some(v) = version_override {
        v.to_string()
    } else {
        local_git_version(&canonical).unwrap_or_else(|| "local".to_string())
    };

    eprintln!(
        "[sempkg] Local source: {} → {}@{}",
        canonical.display(),
        package_name,
        version
    );

    // --- 4. Already installed? ----------------------------------------------
    let store = BundleStore::workspace(workspace_dir);
    if !reinstall && store.is_installed(&package_name, &version) {
        println!(
            "{}@{} is already installed. Use --reinstall to rebuild.",
            package_name, version
        );
        // Still write to manifest if not already present.
        record_local_dep(
            workspace_dir,
            &canonical,
            &package_name,
            &version,
            group,
            include_source,
            source_glob.clone(),
            &source_dirs_override,
            &docs_dirs_override,
            &exclude_dirs,
            description,
            lock,
        )?;
        return Ok(());
    }

    // --- 5. Build -----------------------------------------------------------
    let language = github::detect_language(&canonical);
    let cg_version = codegraph::version();

    eprintln!(
        "[sempkg] Building bundle for {}@{} (language: {language}, codegraph: {cg_version}) ...",
        package_name, version
    );

    let tmp = tempfile::TempDir::new().context("Failed to create temp directory")?;
    let bundle_output = tmp
        .path()
        .join(format!("{}-{}.sembundle", package_name, version));

    let build_opts = sembundle::BuildOptions {
        name: package_name.clone(),
        version: version.clone(),
        source_repo: format!("local:{}", canonical.display()),
        commit_hash: local_git_sha(&canonical).unwrap_or_default(),
        tag: None,
        language,
        codegraph_version: cg_version,
        output_path: Some(bundle_output.clone()),
        source_dirs: if source_dirs_override.is_empty() {
            vec![canonical.clone()]
        } else {
            source_dirs_override
                .iter()
                .map(|d| {
                    if d.is_absolute() {
                        d.clone()
                    } else {
                        canonical.join(d)
                    }
                })
                .collect()
        },
        docs_dirs: if docs_dirs_override.is_empty() {
            vec![canonical.clone()]
        } else {
            docs_dirs_override
                .iter()
                .map(|d| {
                    if d.is_absolute() {
                        d.clone()
                    } else {
                        canonical.join(d)
                    }
                })
                .collect()
        },
        docs_glob: None,
        include_source,
        source_glob: source_glob.clone(),
        exclude_dirs: exclude_dirs
            .iter()
            .map(|d| {
                if d.is_absolute() {
                    d.clone()
                } else {
                    canonical.join(d)
                }
            })
            .collect(),
    };

    sembundle::build(build_opts).with_context(|| {
        format!(
            "Failed to build bundle for {}@{} from '{}'.\n\
             Ensure `codegraph` is on your PATH.",
            package_name,
            version,
            canonical.display()
        )
    })?;

    // --- 6. Install ---------------------------------------------------------
    let bytes = std::fs::read(&bundle_output)
        .with_context(|| format!("Cannot read built bundle at {}", bundle_output.display()))?;

    // Remove existing bundle dir so install_bytes can extract the freshly-built one.
    let existing_dir = store.bundle_dir(&package_name, &version);
    if existing_dir.exists() {
        std::fs::remove_dir_all(&existing_dir).with_context(|| {
            format!(
                "Failed to remove existing bundle at {}",
                existing_dir.display()
            )
        })?;
    }

    let info = store.install_bytes(&bytes)?;

    println!(
        "Installed {}@{} from {}{}{}",
        info.name,
        info.version,
        canonical.display(),
        if info.has_lance() { "  +lance" } else { "" },
        if info.has_code() { "  +code" } else { "" }
    );

    // --- 7. Record in manifest ----------------------------------------------
    record_local_dep(
        workspace_dir,
        &canonical,
        &package_name,
        &version,
        group,
        include_source,
        source_glob,
        &source_dirs_override,
        &docs_dirs_override,
        &exclude_dirs,
        description,
        lock,
    )
}

/// Try to derive a human-readable version from a git repository at `path`.
///
/// Tries `git describe --tags --always --abbrev=12` first (gives a tag like
/// `v1.2.3` or `v1.2.3-42-gabcdef`), then falls back to
/// `git rev-parse --short=12 HEAD`.  Returns `None` if neither succeeds or the
/// directory is not a git repository.
fn local_git_version(path: &Path) -> Option<String> {
    let describe = std::process::Command::new("git")
        .args([
            "-C",
            &path.to_string_lossy(),
            "describe",
            "--tags",
            "--always",
            "--abbrev=12",
        ])
        .output()
        .ok()?;

    if describe.status.success() {
        let v = String::from_utf8_lossy(&describe.stdout).trim().to_string();
        if !v.is_empty() {
            return Some(v.trim_start_matches('v').to_string());
        }
    }

    None
}

/// Try to get the full commit SHA for the HEAD of a git repository at `path`.
fn local_git_sha(path: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["-C", &path.to_string_lossy(), "rev-parse", "HEAD"])
        .output()
        .ok()?;
    if out.status.success() {
        let sha = String::from_utf8_lossy(&out.stdout).trim().to_lowercase();
        if sha.len() == 40 {
            return Some(sha);
        }
    }

    None
}

// ---------------------------------------------------------------------------
// pkg sub-commands
// ---------------------------------------------------------------------------

/// Write the `{ local = "...", version = "..." }` entry into `sempkg.toml`
/// and update `sempkg.lock`.
fn record_local_dep(
    workspace_dir: &Path,
    canonical: &Path,
    package_name: &str,
    version: &str,
    group: Option<&str>,
    include_source: bool,
    source_glob: Option<String>,
    source_dirs_override: &[PathBuf],
    docs_dirs_override: &[PathBuf],
    exclude_dirs: &[PathBuf],
    description: Option<String>,
    lock: Option<&mut manifest::LockFile>,
) -> Result<()> {
    let mut mf = manifest::load_manifest(workspace_dir)?;

    // Prefer a path relative to the workspace directory so the manifest is
    // portable when the whole repo is cloned into a different location.
    //
    // Canonicalize the workspace dir too before computing the relative path:
    // on Windows `canonical` carries a `\\?\` verbatim prefix (from
    // `canonicalize()`) that a non-canonical `workspace_dir` lacks, which would
    // otherwise make `strip_prefix` fail and fall back to an absolute,
    // machine-specific path. When the local source *is* the workspace (the
    // `sempkg add .` case) the relative path is empty, so record `.` instead.
    let canonical_workspace = workspace_dir
        .canonicalize()
        .unwrap_or_else(|_| workspace_dir.to_path_buf());
    let stored_path: String = match canonical.strip_prefix(&canonical_workspace) {
        Ok(rel) if rel.as_os_str().is_empty() => ".".to_string(),
        // Normalize separators to `/` so the recorded path is portable across
        // platforms (forward slashes are accepted by `Path::join` on Windows).
        Ok(rel) => rel.to_string_lossy().replace('\\', "/"),
        Err(_) => canonical.to_string_lossy().into_owned(),
    };

    let dep = manifest::DependencyEntry {
        version: version.to_string(),
        registry: None,
        url: None,
        git: None,
        git_ref: None,
        subdir: None,
        full: false,
        local: Some(stored_path.clone()),
        include_source,
        source_glob,
        source_dirs: source_dirs_override
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        docs_dirs: docs_dirs_override
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        exclude_dirs: exclude_dirs
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        description,
    };
    insert_dep(&mut mf, package_name, dep, group);
    manifest::save_manifest(&mf, workspace_dir)?;

    record_installed_bundle_lock(
        workspace_dir,
        lock,
        package_name,
        version,
        format!("local:{}", stored_path),
        false,
        local_git_sha(canonical),
    )?;

    if let Some(g) = group {
        println!(
            "Recorded {}@{} in group '{}' in sempkg.toml.",
            package_name, version, g
        );
    } else {
        println!("Recorded {}@{} in sempkg.toml.", package_name, version);
    }

    Ok(())
}

mod cli;
mod codegraph;
mod error;
mod manifest;
mod mcp;
mod packages;
mod lance;
mod registry;
mod reranker;
mod store;
mod verify;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;

use sha2::{Digest, Sha256};

use crate::cli::{Cli, Commands, PkgCommands, RerankerCommands};
use crate::manifest::{DependencyEntry, RegistryEntry};
use crate::packages::PackageRegistry;
use crate::registry::{RegistryClient, download_from_url};
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
                    let lance_flag = if b.has_lance() { "  +lance" } else { "" };
                    let scope = match b.scope {
                        BundleScope::Workspace => "workspace",
                        BundleScope::Global    => "global",
                    };
                    println!(
                        "  {:<20} @ {:<12} [{idx}]  [{scope}]{lance_flag}",
                        b.name, b.version
                    );
                }
            }

            Ok(())
        }

        // -----------------------------------------------------------------------
        // Add dependency to manifest
        // -----------------------------------------------------------------------
        Commands::Add { spec, registry_url, registry, group, url } => {
            let dir = require_workspace(workspace)?;
            let (name, version) = parse_spec(&spec)?;

            let mut mf = manifest::load_manifest(dir)?;

            if let Some(direct_url) = url {
                // Direct URL dependency — no registry needed
                let dep = DependencyEntry {
                    version: version.to_string(),
                    registry: None,
                    url: Some(direct_url),
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
                let dep = DependencyEntry { version: version.to_string(), registry: Some(reg_name), url: None };
                insert_dep(&mut mf, name, dep, group.as_deref());
            } else {
                if mf.registries.is_empty() {
                    anyhow::bail!(
                        "No registries defined in sempkg.toml. \
                         Use --registry-url to add one: sempkg add --registry-url <url> {spec}\n\
                         Or use --url to add a direct download URL: sempkg add --url <url> {spec}"
                    );
                }
                // Resolve the registry name: use --registry if given, otherwise
                // default to the first defined registry so the saved TOML is explicit.
                let resolved_registry = registry
                    .or_else(|| mf.default_registry().map(|r| r.name.clone()));
                let dep = DependencyEntry { version: version.to_string(), registry: resolved_registry, url: None };
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
        // Remove dependency from manifest
        // -----------------------------------------------------------------------
        Commands::Remove { name, group } => {
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

            if removed {
                manifest::save_manifest(&mf, dir)?;
                if let Some(g) = &group {
                    println!("Removed '{name}' from group '{g}' in sempkg.toml.");
                } else {
                    println!("Removed '{name}' from sempkg.toml.");
                }
            } else {
                let hint = if group.is_none() {
                    format!(" Use --group <name> to remove from a specific group.")
                } else {
                    String::new()
                };
                anyhow::bail!("'{name}' not found in sempkg.toml.{hint}");
            }
            Ok(())
        }

        // -----------------------------------------------------------------------
        // Sync — install all workspace manifest dependencies
        // -----------------------------------------------------------------------
        Commands::Sync { reinstall, group, all_groups } => {
            let dir = require_workspace(workspace)?;
            let mf = manifest::load_manifest(dir)?;
            let mut lock = manifest::load_lock(dir)?;
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

            let mut installed = Vec::new();

            for (dep_name, dep) in &deps {
                if !reinstall && store.is_installed(dep_name, &dep.version) {
                    println!("  {dep_name}@{} already installed, skipping.", dep.version);
                    continue;
                }

                let bytes: Vec<u8>;
                let source_label: String;

                if let Some(direct_url) = &dep.url {
                    // Direct URL dependency (e.g. GitHub release asset)
                    print!("  Installing {dep_name}@{} from URL ... ", dep.version);
                    std::io::Write::flush(&mut std::io::stdout())?;
                    bytes = download_from_url(direct_url, None)?;
                    source_label = direct_url.clone();
                } else {
                    let reg = mf.registry_for(dep).with_context(|| {
                        format!("No registry found for dependency '{dep_name}'")
                    })?;

                    print!("  Installing {dep_name}@{} from {} ... ", dep.version, reg.name);
                    std::io::Write::flush(&mut std::io::stdout())?;

                    let client = RegistryClient::new(&reg.url);
                    let index_entry = client.lookup(dep_name, &dep.version).ok();
                    let expected_sha256 = index_entry.as_ref().and_then(|e| e.sha256.as_deref());
                    bytes = client.download_bundle(dep_name, &dep.version, expected_sha256)?;
                    source_label = reg.url.clone();

                    // Signature verification
                    if let Some(key) = &verify_key {
                        let sig = client.download_signature(dep_name, &dep.version)?;
                        verify::verify_bundle_signature(&bytes, &sig, key)
                            .with_context(|| format!("Signature verification failed for {dep_name}@{}", dep.version))?;
                    }
                }

                let info = store.install_bytes(&bytes)?;
                println!("done.");

                lock.upsert(manifest::LockEntry {
                    name: dep_name.clone(),
                    version: dep.version.clone(),
                    registry_url: source_label,
                    sha256: hex::encode(Sha256::digest(&bytes)),
                    signed: verify_key.is_some() && dep.url.is_none(),
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
        Commands::Install { spec, global, registry: reg_url, verify_key, url } => {
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
                let reg_url = reg_url.as_deref().expect("--registry required when --url is not provided");
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
                    eprintln!("Warning: signature verification is not supported for direct URL installs.");
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
                "Installed {}@{} [{scope_label}]{}",
                info.name, info.version,
                if info.has_lance() { "  +lance" } else { "" }
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
                println!("  Lance:      {}", bundle.has_lance());
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
        // LanceDB doc search
        // -----------------------------------------------------------------------
        Commands::Docs { package, query, limit } => {
            let bundle_dir = resolve_lance_path(&package, workspace)?;
            let results = lance::search(&bundle_dir, &query, limit)?;
            println!("{}", lance::format_results(&results, &query));
            Ok(())
        }

        // -----------------------------------------------------------------------
        // Hybrid search with reranking
        // -----------------------------------------------------------------------
        Commands::Query { package, query, docs, code, kind, limit, top_k } => {
            run_query(&package, &query, docs, code, kind.as_deref(), limit, top_k, workspace)
        }

        Commands::DocsMeta { package } => {
            let bundle_dir = resolve_lance_path(&package, workspace)?;
            if let Some(meta) = lance::load_metadata(&bundle_dir) {
                println!("LanceDB metadata for '{package}':");
                println!("  Table:       {}", meta.table_name.as_deref().unwrap_or("docs"));
                println!("  Documents:   {}", meta.document_count.unwrap_or(0));
                println!("  Chunks:      {}", meta.chunk_count.unwrap_or(0));
                println!("  FTS enabled: {}", meta.fts_enabled.unwrap_or(false));
                println!("  Indexed at:  {}", meta.created_at.as_deref().unwrap_or("unknown"));
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
    #[cfg(feature = "reranker")]
    {
        let mut cfg: reranker::RerankerConfig = workspace
            .and_then(|d| manifest::load_manifest(d).ok())
            .and_then(|mf| mf.reranker)
            .unwrap_or_default();

        if let Some(k) = top_k_override {
            cfg.top_k = k;
        }
        cfg.output_n = limit;

        if !reranker::model_is_present(&cfg) {
            anyhow::bail!(
                "Reranker model not found. Run `sempkg reranker pull` to download it."
            );
        }

        let mut code_candidates: Vec<reranker::RerankCandidate> = Vec::new();
        if !docs_only {
            match resolve_codegraph_path(package, workspace) {
                Ok(path) => match codegraph::query(&path, query, kind, cfg.top_k) {
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
                Ok(lance_dir) => match lance::search(&lance_dir, query, cfg.top_k) {
                    Ok(results) => doc_candidates.extend(reranker::lance_results_to_candidates(&results)),
                    Err(e) => eprintln!("Warning: doc search failed: {e}"),
                },
                Err(_) => {} // package may be symbols-only; not fatal
            }
        }

        // `top_k` is the total reranker budget across the merged hybrid pool,
        // not per backend. Interleave sources so one side can't monopolize the
        // pool when both are available.
        let candidates = merge_query_candidates(code_candidates, doc_candidates, cfg.top_k);

        if candidates.is_empty() {
            println!("No results for '{query}'.");
            return Ok(());
        }

        // `top_k` applies to the fetch size per backend. Once symbol and doc
        // candidates are merged for hybrid query, score the full combined pool.
        cfg.top_k = candidates.len();

        // Score the full gathered pool first, then apply source-diversity
        // selection and final truncation below.
        cfg.output_n = candidates.len();

        let has_codegraph = candidates.iter().any(|c| c.origin == reranker::RerankOrigin::Codegraph);
        let has_docs = candidates.iter().any(|c| c.origin == reranker::RerankOrigin::Docs);

        eprintln!("Scoring {} candidates...", candidates.len());
        let mut ranker = reranker::Reranker::load(&cfg)?;
        let scored = ranker.rerank(query, candidates)?;
        let scored = diversify_query_results(scored, limit, has_codegraph, has_docs);

        if scored.is_empty() {
            println!("No results for '{query}'.");
        } else {
            println!("{}", reranker::format_reranked_docs(&scored, query));
        }
        Ok(())
    }
    #[cfg(not(feature = "reranker"))]
    {
        let _ = (package, query, docs_only, code_only, kind, limit, top_k_override, workspace);
        anyhow::bail!(
            "The `query` command requires reranker support. \
             Rebuild with `cargo build --features reranker`."
        )
    }
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
        if !final_results.iter().any(|r| r.source == doc.source && r.text == doc.text) {
            final_results.push(doc);
        }
    }

    for result in scored {
        if final_results.len() >= limit {
            break;
        }
        if final_results.iter().any(|r| r.source == result.source && r.text == result.text) {
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
            // Allow URL overrides via CLI flags for custom quants / mirrors.
            let mut pull_cfg = cfg.clone();
            if let Some(url) = &gguf_url {
                pull_cfg.model = Some(url.clone());
            }

            let token = hf_token.as_deref();

            println!("Pulling Qwen3-Reranker-0.6B GGUF model...");
            reranker::pull_model(&pull_cfg, token)?;

            println!();
            println!(
                "Model ready. Add the following to your sempkg.toml to activate reranking:\n\n\
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
            #[cfg(feature = "reranker")]
            {
                if !reranker::model_is_present(&cfg) {
                    anyhow::bail!(
                        "Model not found. Run `sempkg reranker pull` first."
                    );
                }
                println!("Loading Qwen3-Reranker...");
                let mut ranker = reranker::Reranker::load(&cfg)?;
                let candidates = vec![reranker::RerankCandidate {
                    source: "test-document".to_string(),
                    text: document.clone(),
                    origin: reranker::RerankOrigin::Docs,
                }];
                let results = ranker.rerank(&query, candidates)?;
                if let Some(r) = results.first() {
                    println!("Score: {:.4}  (1.0 = highly relevant, 0.0 = not relevant)", r.score);
                } else {
                    println!("No results.");
                }
                Ok(())
            }
            #[cfg(not(feature = "reranker"))]
            {
                let _ = (query, document);
                anyhow::bail!(
                    "Reranker support is not compiled into this binary. \
                     Rebuild with `cargo build --features reranker`."
                )
            }
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

        PkgCommands::LanceIndex { name, pattern } => {
            let reg = PackageRegistry::load()?;
            let pkg = reg.get(&name).with_context(|| format!("Package '{name}' not found."))?;
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

/// Insert a dependency into the manifest, either into [dependencies] or a named group.
fn insert_dep(
    mf: &mut manifest::WorkspaceManifest,
    name: &str,
    dep: manifest::DependencyEntry,
    group: Option<&str>,
) {
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

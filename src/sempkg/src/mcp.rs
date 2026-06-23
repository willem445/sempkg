/// MCP server — JSON-RPC 2.0 over stdio, exposing codegraph + LanceDB tools.
///
/// Protocol: https://spec.modelcontextprotocol.io
/// Transport: stdin/stdout (newline-delimited JSON)
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use std::cell::RefCell;

use crate::packages::PackageRegistry;
use crate::reranker::{self, Reranker, RerankerConfig};
use crate::store::{list_all_bundles, resolve_bundle};
use crate::{codegraph, lance};

// ---------------------------------------------------------------------------
// JSON-RPC types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RpcId {
    Number(i64),
    String(String),
    Null,
}

impl Serialize for RpcId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            RpcId::Number(n) => s.serialize_i64(*n),
            RpcId::String(st) => s.serialize_str(st),
            RpcId::Null => s.serialize_none(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct RpcRequest {
    #[allow(dead_code)]
    jsonrpc: String,
    id: Option<RpcId>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize)]
struct RpcResponse {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

#[derive(Debug, Serialize)]
struct RpcError {
    code: i32,
    message: String,
}

impl RpcResponse {
    fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }
    fn err(id: Value, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Tool definitions
// ---------------------------------------------------------------------------

fn tool_schema(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "required": required,
        }
    })
}

fn all_tools() -> Value {
    json!([
        tool_schema(
            "list_packages",
            "List all registered local packages and installed bundles with their index and LanceDB doc status.",
            json!({}),
            &[]
        ),
        tool_schema(
            "search_symbols",
            "Search for symbols (functions, classes, variables) in a specific package using CodeGraph. \
             Results are scoped exclusively to the named package.",
            json!({
                "package": { "type": "string", "description": "Package or bundle name" },
                "query":   { "type": "string", "description": "Symbol name or pattern to search" },
                "kind":    { "type": "string", "description": "Optional symbol kind filter (function, class, variable, ...)" },
                "limit":   { "type": "integer", "description": "Max results (default 20)" }
            }),
            &["package", "query"]
        ),
        tool_schema(
            "get_context",
            "Get AI-optimised code context for a task description, scoped to a specific package.",
            json!({
                "package": { "type": "string", "description": "Package or bundle name" },
                "task":    { "type": "string", "description": "Natural-language description of the task" },
                "limit":   { "type": "integer", "description": "Max symbols to return (default 20)" }
            }),
            &["package", "task"]
        ),
        tool_schema(
            "get_callers",
            "Find all callers of a symbol in a specific package.",
            json!({
                "package": { "type": "string", "description": "Package or bundle name" },
                "symbol":  { "type": "string", "description": "Fully-qualified or short symbol name" },
                "limit":   { "type": "integer", "description": "Max results (default 20)" }
            }),
            &["package", "symbol"]
        ),
        tool_schema(
            "get_callees",
            "Find all functions/methods called by a symbol in a specific package.",
            json!({
                "package": { "type": "string", "description": "Package or bundle name" },
                "symbol":  { "type": "string", "description": "Fully-qualified or short symbol name" },
                "limit":   { "type": "integer", "description": "Max results (default 20)" }
            }),
            &["package", "symbol"]
        ),
        tool_schema(
            "get_impact",
            "Analyse the downstream impact of changing a symbol (what breaks if this changes).",
            json!({
                "package": { "type": "string", "description": "Package or bundle name" },
                "symbol":  { "type": "string", "description": "Symbol to analyse" },
                "depth":   { "type": "integer", "description": "Call graph depth (default 3)" }
            }),
            &["package", "symbol"]
        ),
        tool_schema(
            "list_files",
            "List source files tracked by CodeGraph in a specific package. \
             The optional filter supports both glob patterns (e.g. **/*.rs, src/**/mod.rs) \
             and plain case-insensitive substring matching (e.g. auth, storage). \
             Patterns containing * or ? are treated as globs; all other values are substrings. \
             A 'No files matched' response means the filter was valid but nothing matched — \
             try a shorter substring or broader glob. \
             A 'Filter error' response means the glob pattern was syntactically invalid. \
             Use limit to cap the number of results (default 200).",
            json!({
                "package": { "type": "string", "description": "Package or bundle name" },
                "filter":  { "type": "string", "description": "Optional glob pattern (e.g. **/*.rs) or substring (e.g. auth)" },
                "limit":   { "type": "integer", "description": "Max files to return (default 200)" }
            }),
            &["package"]
        ),
        tool_schema(
            "search_docs",
            "Search the LanceDB documentation index for a specific bundle. \
             Returns BM25-ranked excerpts scoped to that bundle only.",
            json!({
                "package": { "type": "string", "description": "Bundle name" },
                "query":   { "type": "string", "description": "Documentation search query" },
                "limit":   { "type": "integer", "description": "Max results (default 10)" }
            }),
            &["package", "query"]
        ),
        tool_schema(
            "docs_metadata",
            "Show LanceDB metadata (table name, document count, chunk count, FTS status) for a bundle.",
            json!({
                "package": { "type": "string", "description": "Bundle name" }
            }),
            &["package"]
        ),
        tool_schema(
            "search_code",
            "Search the embedded source-code index for a bundle. \
             Returns BM25-ranked symbol excerpts with their file location, \
             kind, and signature. Only available for bundles built with --include-source.",
            json!({
                "package": { "type": "string", "description": "Bundle name" },
                "query":   { "type": "string", "description": "Natural-language or keyword code search query" },
                "kind":    { "type": "string", "description": "Optional kind filter: function, class, struct, enum, trait, impl, ..." },
                "limit":   { "type": "integer", "description": "Max results (default 10)" }
            }),
            &["package", "query"]
        ),
        tool_schema(
            "read_symbol",
            "Fetch the full source body of a named symbol from the embedded code index. \
             Returns the complete implementation, file path, and line range. \
             Only available for bundles built with --include-source.",
            json!({
                "package": { "type": "string", "description": "Bundle name" },
                "symbol":  { "type": "string", "description": "Symbol name to look up (short or qualified)" }
            }),
            &["package", "symbol"]
        ),
        tool_schema(
            "read_code",
            "Read the exact source body of the symbol that contains a given file and line number. \
             Use this after search_symbols, get_callers, get_callees, or get_impact return a \
             file path and line number — pass those directly here to retrieve the precise \
             implementation without doing a secondary search. \
             Only available for bundles built with --include-source.",
            json!({
                "package": { "type": "string", "description": "Bundle name" },
                "file":    { "type": "string", "description": "Source file path as returned by codegraph (e.g. src/foo.rs)" },
                "line":    { "type": "integer", "description": "Line number within that file (1-based)" }
            }),
            &["package", "file", "line"]
        ),
    ])
}

// ---------------------------------------------------------------------------
// Tool implementations
// ---------------------------------------------------------------------------

struct McpContext {
    workspace_dir: Option<PathBuf>,
    registry: PackageRegistry,
    /// Optionally-loaded Qwen3 reranker. Wrapped in RefCell so tool methods
    /// can mutably borrow it through an immutable &McpContext reference.
    reranker: RefCell<Option<Reranker>>,
    reranker_cfg: Option<RerankerConfig>,
}

impl McpContext {
    fn new(workspace_dir: Option<PathBuf>) -> Self {
        let registry = PackageRegistry::load().unwrap_or_default();

        // Try to load reranker config from the workspace manifest.
        let reranker_cfg: Option<RerankerConfig> = workspace_dir
            .as_deref()
            .and_then(|d| crate::manifest::load_manifest(d).ok())
            .and_then(|mf| mf.reranker);

        // Eagerly load the model when it's enabled and the files exist.
        let reranker = reranker_cfg.as_ref().and_then(|cfg| {
            if cfg.enabled && reranker::model_is_present(cfg) {
                match Reranker::load(cfg) {
                    Ok(r) => {
                        eprintln!(
                            "sempkg: reranker loaded ({} top_k, {} output_n)",
                            cfg.top_k, cfg.output_n
                        );
                        Some(r)
                    }
                    Err(e) => {
                        eprintln!("sempkg: reranker load error (falling back to BM25): {e}");
                        None
                    }
                }
            } else {
                None
            }
        });

        Self {
            workspace_dir,
            registry,
            reranker: RefCell::new(reranker),
            reranker_cfg,
        }
    }

    fn workspace(&self) -> Option<&PathBuf> {
        self.workspace_dir.as_ref()
    }

    /// Resolve a package to its codegraph project path.
    /// Checks local registry packages first, then workspace bundles, then global bundles.
    fn resolve_codegraph_path(&self, name: &str) -> Result<PathBuf, String> {
        // Local registered package
        if let Some(pkg) = self.registry.get(name) {
            if !pkg.is_indexed() {
                return Err(format!(
                    "Package '{name}' is registered but not indexed. \
                     Run 'sempkg reindex {name}' first."
                ));
            }
            return Ok(pkg.abs_path());
        }

        // Installed bundle (workspace-first)
        if let Some(bundle) = resolve_bundle(name, self.workspace().map(|p| p.as_path())) {
            if !bundle.is_indexed() {
                return Err(format!(
                    "Bundle '{name}@{}' is installed but has no codegraph index.",
                    bundle.version
                ));
            }
            return Ok(bundle.bundle_dir);
        }

        let available = self.available_names();
        let hint = if available.is_empty() {
            " No packages or bundles installed yet.".to_string()
        } else {
            format!(" Available: {}", available.join(", "))
        };
        Err(format!("Package '{name}' not found.{hint}"))
    }

    /// Resolve a package/bundle name to its LanceDB directory path.
    /// Checks local packages first (scoped index), then installed bundles.
    fn resolve_lance_path(&self, name: &str) -> Result<PathBuf, String> {
        // Local package with a scoped LanceDB index at <pkg>/.sempkg/lance/
        if let Some(pkg) = self.registry.get(name) {
            let lance_dir = pkg.abs_path().join(".sempkg").join("lance");
            if lance_dir.is_dir() {
                return Ok(lance_dir);
            }
            return Err(format!(
                "Package '{name}' has no LanceDB index. Run 'sempkg pkg lance-index {name}' to build one."
            ));
        }

        // Installed bundle
        if let Some(bundle) = resolve_bundle(name, self.workspace().map(|p| p.as_path())) {
            if !bundle.has_lance() {
                return Err(format!(
                    "Bundle '{name}@{}' does not have a LanceDB documentation index.",
                    bundle.version
                ));
            }
            return Ok(bundle.bundle_dir.join("lance"));
        }

        Err(format!(
            "'{name}' not found. Use 'sempkg list' to see available packages and bundles."
        ))
    }

    fn available_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .registry
            .list()
            .iter()
            .map(|p| p.name.clone())
            .collect();
        names.extend(
            list_all_bundles(self.workspace().map(|p| p.as_path()))
                .into_iter()
                .map(|b| format!("{}@{}", b.name, b.version)),
        );
        names
    }

    fn tool_list_packages(&self) -> String {
        let local_pkgs = self.registry.list();
        let bundles = list_all_bundles(self.workspace().map(|p| p.as_path()));

        if local_pkgs.is_empty() && bundles.is_empty() {
            return "No packages or bundles registered.\n\
                    Use 'sempkg pkg add <name> <path>' for local packages.\n\
                    Use 'sempkg sync' or 'sempkg install <name>@<version>' for bundles."
                .to_string();
        }

        let mut lines = Vec::new();

        if !local_pkgs.is_empty() {
            lines.push("**Local packages:**".to_string());
            for pkg in &local_pkgs {
                let idx = if pkg.is_indexed() {
                    "indexed"
                } else {
                    "NOT indexed"
                };
                let desc = if pkg.description.is_empty() {
                    String::new()
                } else {
                    format!("  — {}", pkg.description)
                };
                lines.push(format!(
                    "  • **{}** [{}]  {}{}",
                    pkg.name, idx, pkg.path, desc
                ));
            }
        }

        if !bundles.is_empty() {
            if !local_pkgs.is_empty() {
                lines.push(String::new());
            }
            lines.push("**Installed bundles:**".to_string());
            for b in &bundles {
                let idx = if b.is_indexed() {
                    "indexed"
                } else {
                    "no graph"
                };
                let lance = if b.has_lance() { "  +lance" } else { "" };
                let code = if b.has_code() { "  +code" } else { "" };
                let scope = match b.scope {
                    crate::store::BundleScope::Workspace => "workspace",
                    crate::store::BundleScope::Global => "global",
                };
                lines.push(format!(
                    "  \u{2022} **{}** @ {}  [{}]  [{}]{}{}",
                    b.name, b.version, idx, scope, lance, code
                ));
            }
        }

        lines.join("\n")
    }

    fn tool_search_symbols(
        &self,
        package: &str,
        query: &str,
        kind: Option<&str>,
        limit: usize,
    ) -> String {
        match self.resolve_codegraph_path(package) {
            Err(e) => e,
            Ok(path) => {
                // When a reranker is present, fetch more candidates than
                // requested so the model has a richer pool to score.
                let fetch_limit = self.reranker_fetch_limit(limit);
                let raw = codegraph::query(&path, query, kind, fetch_limit)
                    .unwrap_or_else(|e| format!("Error: {e}"));

                self.apply_rerank_to_codegraph(query, &raw, limit)
            }
        }
    }

    fn tool_get_context(&self, package: &str, task: &str, limit: usize) -> String {
        let path = match self.resolve_codegraph_path(package) {
            Err(e) => return e,
            Ok(p) => p,
        };

        // Fetch more candidates than `limit` so the reranker has a richer pool.
        let fetch_limit = self.reranker_fetch_limit(limit).max(limit * 2);

        // Request JSON output with code blocks suppressed so we can rerank the
        // symbol list before returning it.
        let raw = match codegraph::context_json(&path, task, fetch_limit) {
            Ok(s) => s,
            Err(_) => {
                // Graceful fallback: return plain markdown output.
                return codegraph::context(&path, task)
                    .unwrap_or_else(|e| format!("Error: {e}"));
            }
        };

        // Parse the JSON response: extract the `nodes` array and re-serialise
        // it as a plain array so `codegraph_json_to_candidates` can consume it.
        let nodes_json: String = match serde_json::from_str::<serde_json::Value>(&raw) {
            Ok(v) => {
                let nodes = v.get("nodes").cloned().unwrap_or(serde_json::Value::Array(vec![]));
                serde_json::to_string(&nodes).unwrap_or_default()
            }
            Err(_) => return raw, // not JSON — return as-is
        };

        self.apply_rerank_to_codegraph(task, &nodes_json, limit)
    }

    fn tool_get_callers(&self, package: &str, symbol: &str, limit: usize) -> String {
        match self.resolve_codegraph_path(package) {
            Err(e) => e,
            Ok(path) => {
                let raw = codegraph::callers(&path, symbol, limit)
                    .unwrap_or_else(|e| format!("Error: {e}"));
                self.augment_with_source(package, &raw)
            }
        }
    }

    fn tool_get_callees(&self, package: &str, symbol: &str, limit: usize) -> String {
        match self.resolve_codegraph_path(package) {
            Err(e) => e,
            Ok(path) => {
                let raw = codegraph::callees(&path, symbol, limit)
                    .unwrap_or_else(|e| format!("Error: {e}"));
                self.augment_with_source(package, &raw)
            }
        }
    }

    fn tool_get_impact(&self, package: &str, symbol: &str, depth: usize) -> String {
        match self.resolve_codegraph_path(package) {
            Err(e) => e,
            Ok(path) => {
                codegraph::impact(&path, symbol, depth).unwrap_or_else(|e| format!("Error: {e}"))
            }
        }
    }

    fn tool_list_files(&self, package: &str, filter: Option<&str>, limit: usize) -> String {
        match self.resolve_codegraph_path(package) {
            Err(e) => e,
            Ok(path) => codegraph::files(&path, filter, limit).unwrap_or_else(|e| format!("Error: {e}")),
        }
    }

    fn tool_search_docs(&self, package: &str, query: &str, limit: usize) -> String {
        match self.resolve_lance_path(package) {
            Err(e) => e,
            Ok(lance_dir) => {
                let fetch_limit = self.reranker_fetch_limit(limit);
                match lance::search(&lance_dir, query, fetch_limit) {
                    Ok(results) => self.apply_rerank_to_lance(query, results, limit),
                    Err(e) => format!("Error searching docs: {e}"),
                }
            }
        }
    }

    fn tool_search_code(&self, package: &str, query: &str, kind_filter: Option<&str>, limit: usize) -> String {
        match self.resolve_code_path(package) {
            Err(e) => e,
            Ok(code_dir) => {
                let fetch_limit = self.reranker_fetch_limit(limit);
                match lance::search_code(&code_dir, query, fetch_limit) {
                    Err(e) => format!("Error searching code: {e}"),
                    Ok(mut results) => {
                        // Client-side kind filter
                        if let Some(k) = kind_filter {
                            results.retain(|r| {
                                r.kind.as_deref().map_or(false, |rk| rk == k)
                            });
                        }
                        self.apply_rerank_to_lance(query, results, limit)
                    }
                }
            }
        }
    }

    fn tool_read_code(&self, package: &str, file: &str, line: u32) -> String {
        match self.resolve_code_path(package) {
            Err(e) => e,
            Ok(code_dir) => {
                match lance::fetch_symbol_at_location(&code_dir, file, line) {
                    Err(e) => format!("Error reading code: {e}"),
                    Ok(None) => format!(
                        "No symbol found covering {file}:{line} in the code index for '{package}'. \
                         Verify the file path and line number from the codegraph results, or use \
                         read_symbol to look up by name."
                    ),
                    Ok(Some(src)) => {
                        let loc = format!("{}:{}-{}", src.path, src.start_line, src.end_line);
                        let sig_part = if src.signature.is_empty() {
                            String::new()
                        } else {
                            format!("\n_{}_", src.signature)
                        };
                        format!(
                            "**{}** ({}) @ {}{}\n\n```\n{}\n```",
                            src.symbol, src.kind, loc, sig_part, src.content
                        )
                    }
                }
            }
        }
    }

    fn tool_read_symbol(&self, package: &str, symbol: &str) -> String {
        match self.resolve_code_path(package) {
            Err(e) => e,
            Ok(code_dir) => {
                match lance::fetch_symbol_source(&code_dir, symbol) {
                    Err(e) => format!("Error reading symbol: {e}"),
                    Ok(lance::SymbolLookup::NotFound) => format!(
                        "Symbol '{symbol}' not found in the code index for '{package}'. \
                         Try search_code to locate it first."
                    ),
                    Ok(lance::SymbolLookup::Ambiguous(candidates)) => {
                        let mut msg = format!(
                            "**'{symbol}' is ambiguous** — {n} nodes share this name. \
                             Use `read_code` with a file path and line number to disambiguate.\n\n\
                             | # | Name | Kind | File | Lines |\n\
                             |---|------|------|------|-------|\n",
                            n = candidates.len()
                        );
                        for (i, c) in candidates.iter().enumerate() {
                            let display_name = if c.qualified_name.is_empty() {
                                c.name.clone()
                            } else {
                                c.qualified_name.clone()
                            };
                            msg.push_str(&format!(
                                "| {} | `{}` | {} | {} | {}-{} |\n",
                                i + 1,
                                display_name,
                                c.kind,
                                c.path,
                                c.start_line,
                                c.end_line,
                            ));
                        }
                        msg
                    }
                    Ok(lance::SymbolLookup::Unique(src)) => {
                        let loc = if src.start_line > 0 {
                            format!("{}:{}-{}", src.path, src.start_line, src.end_line)
                        } else {
                            src.path.clone()
                        };
                        format!(
                            "**{}** ({}) @ {}\n\n```\n{}\n```",
                            src.symbol, src.kind, loc, src.content
                        )
                    }
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Reranker helpers
    // -----------------------------------------------------------------------

    /// Returns the candidate fetch limit: top_k from config when a reranker
    /// is available, otherwise the caller-supplied `limit`.
    fn reranker_fetch_limit(&self, limit: usize) -> usize {
        if let Some(cfg) = &self.reranker_cfg {
            if cfg.enabled && self.reranker.borrow().is_some() {
                return cfg.top_k.max(limit);
            }
        }
        limit
    }

    /// Rerank raw codegraph JSON output (array of symbol objects).
    /// Falls back to returning the raw string unchanged on any error.
    fn apply_rerank_to_codegraph(&self, query: &str, raw_json: &str, output_n: usize) -> String {
        let mut guard = self.reranker.borrow_mut();
        let Some(ranker) = guard.as_mut() else {
            return raw_json.to_string();
        };

        let candidates = reranker::codegraph_json_to_candidates(raw_json);
        if candidates.is_empty() {
            return raw_json.to_string();
        }

        match ranker.rerank(query, candidates) {
            Ok(mut scored) => {
                scored.truncate(output_n);
                if scored.is_empty() {
                    return format!("No results for '{query}'.");
                }
                let header = format!(
                    "_Reranked {count} results with Qwen3-Reranker (top {output_n} shown):_\n\n",
                    count = scored.len()
                );
                header + &reranker::format_reranked_docs(&scored, query)
            }
            Err(e) => {
                eprintln!("sempkg: reranker error ({e}), returning BM25 results");
                raw_json.to_string()
            }
        }
    }

    /// Rerank LanceDB search results.
    fn apply_rerank_to_lance(
        &self,
        query: &str,
        results: Vec<lance::SearchResult>,
        output_n: usize,
    ) -> String {
        let mut guard = self.reranker.borrow_mut();
        let Some(ranker) = guard.as_mut() else {
            return lance::format_results(&results, query);
        };

        let candidates = reranker::lance_results_to_candidates(&results);
        if candidates.is_empty() {
            return lance::format_results(&results, query);
        }

        match ranker.rerank(query, candidates) {
            Ok(mut scored) => {
                scored.truncate(output_n);
                let header = format!(
                    "_Reranked {count} docs with Qwen3-Reranker (top {output_n} shown):_\n\n",
                    count = scored.len()
                );
                header + &reranker::format_reranked_docs(&scored, query)
            }
            Err(e) => {
                eprintln!("sempkg: reranker error ({e}), returning BM25 results");
                lance::format_results(&results, query)
            }
        }
    }

    fn tool_docs_metadata(&self, package: &str) -> String {
        match self.resolve_lance_path(package) {
            Err(e) => e,
            Ok(lance_dir) => {
                // load_metadata expects bundle_dir (parent of lance/), but lance_dir IS the lance dir
                // so we read metadata.json directly
                let meta_path = lance_dir.join("metadata.json");
                match std::fs::read_to_string(&meta_path)
                    .ok()
                    .and_then(|s| serde_json::from_str::<lance::LanceMetadata>(&s).ok())
                {
                    Some(meta) => format!(
                        "**LanceDB metadata for '{package}'**\n\
                         - Table: {}\n\
                         - Documents: {}\n\
                         - Chunks: {}\n\
                         - FTS enabled: {}\n\
                         - Indexed at: {}",
                        meta.table_name.as_deref().unwrap_or("docs"),
                        meta.document_count.unwrap_or(0),
                        meta.chunk_count.unwrap_or(0),
                        meta.fts_enabled.unwrap_or(false),
                        meta.created_at.as_deref().unwrap_or("unknown"),
                    ),
                    None => format!("No LanceDB metadata found for bundle '{package}'."),
                }
            }
        }
    }

    /// Resolve a package/bundle name to its code-index directory (`code/`).
    fn resolve_code_path(&self, name: &str) -> Result<PathBuf, String> {
        if let Some(bundle) = resolve_bundle(name, self.workspace().map(|p| p.as_path())) {
            if !bundle.has_code() {
                return Err(format!(
                    "Bundle '{name}@{}' does not have a source-code index. \
                     Rebuild with 'sembundle build --include-source'.",
                    bundle.version
                ));
            }
            return Ok(bundle.bundle_dir.join("code"));
        }
        Err(format!(
            "'{name}' not found or has no source-code index. \
             Use 'sempkg list' to see available bundles."
        ))
    }

    /// When a code index is available, parse callee/caller JSON and append
    /// the full source body for each returned symbol.
    fn augment_with_source(&self, package: &str, codegraph_json: &str) -> String {
        let code_dir = match self.resolve_code_path(package) {
            Ok(d) => d,
            Err(_) => return codegraph_json.to_string(), // no code index, return as-is
        };

        // Parse JSON array of { "node": { "qualifiedName": ..., "name": ... }, ... }
        let arr = match serde_json::from_str::<Vec<serde_json::Value>>(codegraph_json) {
            Ok(a) => a,
            Err(_) => return codegraph_json.to_string(),
        };

        const BYTE_BUDGET: usize = 12_000;
        let mut total_bytes = 0usize;
        let mut sections: Vec<String> = Vec::new();

        for v in &arr {
            let node = v.get("node").unwrap_or(v);
            let get_str = |k: &str| node.get(k).and_then(|x| x.as_str()).unwrap_or("");
            let qualified = get_str("qualifiedName");
            let name = get_str("name");
            let sym = if !qualified.is_empty() { qualified } else { name };
            if sym.is_empty() {
                continue;
            }

            if let Ok(lance::SymbolLookup::Unique(src)) = lance::fetch_symbol_source(&code_dir, sym) {
                if total_bytes + src.content.len() > BYTE_BUDGET {
                    // Include signature only once budget is exceeded
                    let sig_line = src.signature.chars().take(120).collect::<String>();
                    if !sig_line.is_empty() {
                        sections.push(format!("**{}** ({}:{}-{}) \u{2014} body omitted (budget)\n_{sig_line}_",
                            src.symbol, src.path, src.start_line, src.end_line));
                    }
                    continue;
                }
                total_bytes += src.content.len();
                let loc = format!("{}:{}-{}", src.path, src.start_line, src.end_line);
                sections.push(format!(
                    "**{}** ({}) @ {}\n\n```\n{}\n```",
                    src.symbol, src.kind, loc, src.content
                ));
            }
        }

        if sections.is_empty() {
            return codegraph_json.to_string();
        }

        format!(
            "{}\n\n---\n\n### Source bodies\n\n{}",
            codegraph_json,
            sections.join("\n\n---\n\n")
        )
    }

    fn dispatch_tool(&self, name: &str, args: &Value) -> String {
        let str_arg = |key: &str| args.get(key).and_then(|v| v.as_str()).unwrap_or_default();
        let int_arg = |key: &str, default: usize| {
            args.get(key)
                .and_then(|v| v.as_u64())
                .unwrap_or(default as u64) as usize
        };
        let opt_str = |key: &str| args.get(key).and_then(|v| v.as_str());

        match name {
            "list_packages" => self.tool_list_packages(),
            "search_symbols" => self.tool_search_symbols(
                str_arg("package"),
                str_arg("query"),
                opt_str("kind"),
                int_arg("limit", 20),
            ),
            "get_context" => self.tool_get_context(
                str_arg("package"),
                str_arg("task"),
                int_arg("limit", 20),
            ),
            "get_callers" => {
                self.tool_get_callers(str_arg("package"), str_arg("symbol"), int_arg("limit", 20))
            }
            "get_callees" => {
                self.tool_get_callees(str_arg("package"), str_arg("symbol"), int_arg("limit", 20))
            }
            "get_impact" => {
                self.tool_get_impact(str_arg("package"), str_arg("symbol"), int_arg("depth", 3))
            }
            "list_files" => self.tool_list_files(str_arg("package"), opt_str("filter"), int_arg("limit", 200)),
            "search_docs" => {
                self.tool_search_docs(str_arg("package"), str_arg("query"), int_arg("limit", 10))
            }
            "docs_metadata" => self.tool_docs_metadata(str_arg("package")),
            "search_code" => self.tool_search_code(
                str_arg("package"),
                str_arg("query"),
                opt_str("kind"),
                int_arg("limit", 10),
            ),
            "read_symbol" => self.tool_read_symbol(str_arg("package"), str_arg("symbol")),
            "read_code" => {
                let line = args
                    .get("line")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32;
                self.tool_read_code(str_arg("package"), str_arg("file"), line)
            }
            _ => format!("Unknown tool: {name}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Server loop
// ---------------------------------------------------------------------------

pub fn run(workspace_dir: Option<PathBuf>) -> Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    let ctx = McpContext::new(workspace_dir);

    eprintln!("sempkg MCP server ready (stdio transport)");

    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let response = handle_message(&ctx, &line);
        if let Some(resp) = response {
            let json = serde_json::to_string(&resp)?;
            writeln!(out, "{json}")?;
            out.flush()?;
        }
    }

    Ok(())
}

fn handle_message(ctx: &McpContext, line: &str) -> Option<RpcResponse> {
    let req: RpcRequest = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => {
            return Some(RpcResponse::err(
                Value::Null,
                -32700,
                format!("Parse error: {e}"),
            ));
        }
    };

    let id = match &req.id {
        Some(RpcId::Number(n)) => json!(n),
        Some(RpcId::String(s)) => json!(s),
        Some(RpcId::Null) | None => Value::Null,
    };

    match req.method.as_str() {
        "initialize" => Some(RpcResponse::ok(
            id,
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "tools": {}
                },
                "serverInfo": {
                    "name": "sempkg",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "instructions": "sempkg exposes CodeGraph and QMD intelligence for registered packages and installed bundles. All queries are package-scoped. Start with list_packages to discover available packages and bundles."
            }),
        )),

        "notifications/initialized" => {
            // No response required for notifications
            None
        }

        "tools/list" => Some(RpcResponse::ok(id, json!({ "tools": all_tools() }))),

        "tools/call" => {
            let tool_name = req
                .params
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let args = req.params.get("arguments").cloned().unwrap_or(json!({}));

            let text = ctx.dispatch_tool(tool_name, &args);

            Some(RpcResponse::ok(
                id,
                json!({
                    "content": [{ "type": "text", "text": text }]
                }),
            ))
        }

        "ping" => Some(RpcResponse::ok(id, json!({}))),

        _ => Some(RpcResponse::err(
            id,
            -32601,
            format!("Method not found: {}", req.method),
        )),
    }
}

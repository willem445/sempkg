/// MCP server — JSON-RPC 2.0 over stdio, exposing codegraph + LanceDB tools.
///
/// Protocol: https://spec.modelcontextprotocol.io
/// Transport: stdin/stdout (newline-delimited JSON)
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::packages::PackageRegistry;
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
        Self { jsonrpc: "2.0", id, result: Some(result), error: None }
    }
    fn err(id: Value, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(RpcError { code, message: message.into() }),
        }
    }
}

// ---------------------------------------------------------------------------
// Tool definitions
// ---------------------------------------------------------------------------

fn tool_schema(
    name: &str,
    description: &str,
    properties: Value,
    required: &[&str],
) -> Value {
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
                "task":    { "type": "string", "description": "Natural-language description of the task" }
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
            "List source files tracked by CodeGraph in a specific package.",
            json!({
                "package": { "type": "string", "description": "Package or bundle name" },
                "filter":  { "type": "string", "description": "Optional path/glob filter" }
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
    ])
}

// ---------------------------------------------------------------------------
// Tool implementations
// ---------------------------------------------------------------------------

struct McpContext {
    workspace_dir: Option<PathBuf>,
    registry: PackageRegistry,
}

impl McpContext {
    fn new(workspace_dir: Option<PathBuf>) -> Self {
        let registry = PackageRegistry::load().unwrap_or_default();
        Self { workspace_dir, registry }
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

        Err(format!("'{name}' not found. Use 'sempkg list' to see available packages and bundles."))
    }

    fn available_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.registry.list().iter().map(|p| p.name.clone()).collect();
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
                let idx = if pkg.is_indexed() { "indexed" } else { "NOT indexed" };
                let desc = if pkg.description.is_empty() { String::new() } else { format!("  — {}", pkg.description) };
                lines.push(format!("  • **{}** [{}]  {}{}", pkg.name, idx, pkg.path, desc));
            }
        }

        if !bundles.is_empty() {
            if !local_pkgs.is_empty() { lines.push(String::new()); }
            lines.push("**Installed bundles:**".to_string());
            for b in &bundles {
                let idx = if b.is_indexed() { "indexed" } else { "no graph" };
                let lance = if b.has_lance() { "  +lance" } else { "" };
                let scope = match b.scope {
                    crate::store::BundleScope::Workspace => "workspace",
                    crate::store::BundleScope::Global => "global",
                };
                lines.push(format!(
                    "  \u{2022} **{}** @ {}  [{}]  [{}]{}",
                    b.name, b.version, idx, scope, lance
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
            Ok(path) => codegraph::query(&path, query, kind, limit)
                .unwrap_or_else(|e| format!("Error: {e}")),
        }
    }

    fn tool_get_context(&self, package: &str, task: &str) -> String {
        match self.resolve_codegraph_path(package) {
            Err(e) => e,
            Ok(path) => codegraph::context(&path, task)
                .unwrap_or_else(|e| format!("Error: {e}")),
        }
    }

    fn tool_get_callers(&self, package: &str, symbol: &str, limit: usize) -> String {
        match self.resolve_codegraph_path(package) {
            Err(e) => e,
            Ok(path) => codegraph::callers(&path, symbol, limit)
                .unwrap_or_else(|e| format!("Error: {e}")),
        }
    }

    fn tool_get_callees(&self, package: &str, symbol: &str, limit: usize) -> String {
        match self.resolve_codegraph_path(package) {
            Err(e) => e,
            Ok(path) => codegraph::callees(&path, symbol, limit)
                .unwrap_or_else(|e| format!("Error: {e}")),
        }
    }

    fn tool_get_impact(&self, package: &str, symbol: &str, depth: usize) -> String {
        match self.resolve_codegraph_path(package) {
            Err(e) => e,
            Ok(path) => codegraph::impact(&path, symbol, depth)
                .unwrap_or_else(|e| format!("Error: {e}")),
        }
    }

    fn tool_list_files(&self, package: &str, filter: Option<&str>) -> String {
        match self.resolve_codegraph_path(package) {
            Err(e) => e,
            Ok(path) => codegraph::files(&path, filter)
                .unwrap_or_else(|e| format!("Error: {e}")),
        }
    }

    fn tool_search_docs(&self, package: &str, query: &str, limit: usize) -> String {
        match self.resolve_lance_path(package) {
            Err(e) => e,
            Ok(lance_dir) => {
                match lance::search(&lance_dir, query, limit) {
                    Ok(results) => lance::format_results(&results, query),
                    Err(e) => format!("Error searching docs: {e}"),
                }
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

    fn dispatch_tool(&self, name: &str, args: &Value) -> String {
        let str_arg = |key: &str| args.get(key).and_then(|v| v.as_str()).unwrap_or_default();
        let int_arg = |key: &str, default: usize| {
            args.get(key).and_then(|v| v.as_u64()).unwrap_or(default as u64) as usize
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
            "get_context" => self.tool_get_context(str_arg("package"), str_arg("task")),
            "get_callers" => self.tool_get_callers(
                str_arg("package"),
                str_arg("symbol"),
                int_arg("limit", 20),
            ),
            "get_callees" => self.tool_get_callees(
                str_arg("package"),
                str_arg("symbol"),
                int_arg("limit", 20),
            ),
            "get_impact" => self.tool_get_impact(
                str_arg("package"),
                str_arg("symbol"),
                int_arg("depth", 3),
            ),
            "list_files" => self.tool_list_files(str_arg("package"), opt_str("filter")),
            "search_docs" => self.tool_search_docs(
                str_arg("package"),
                str_arg("query"),
                int_arg("limit", 10),
            ),
            "docs_metadata" => self.tool_docs_metadata(str_arg("package")),
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
        "initialize" => {
            Some(RpcResponse::ok(
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
            ))
        }

        "notifications/initialized" => {
            // No response required for notifications
            None
        }

        "tools/list" => {
            Some(RpcResponse::ok(id, json!({ "tools": all_tools() })))
        }

        "tools/call" => {
            let tool_name = req.params.get("name").and_then(|v| v.as_str()).unwrap_or("");
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

        _ => Some(RpcResponse::err(id, -32601, format!("Method not found: {}", req.method))),
    }
}

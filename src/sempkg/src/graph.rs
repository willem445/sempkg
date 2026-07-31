//! Adapter bridging the native [`semgraph`] reader to sempkg's existing
//! codegraph-JSON contract.
//!
//! This is what replaces the CodeGraph **CLI** on the query path (issue #78,
//! Phase 1 wire-up). The MCP formatters/rerankers (`fmt_codegraph_hit`,
//! `codegraph_json_to_candidates`, `fmt_codegraph_with_source`, …) all consume
//! the camelCase JSON the `codegraph` CLI used to emit; these functions produce
//! byte-compatible JSON from `semgraph` results, so no formatter had to change.
//!
//! The CodeGraph CLI is now only invoked by `sempkg index` / `pkg reindex`
//! (see [`crate::codegraph`]); querying an installed bundle needs no Node/
//! CodeGraph install at all.

use std::path::Path;

use anyhow::{Context, Result};
use semgraph::{CallEdge, GraphDb, GraphNode};
use serde_json::{json, Value};

/// Open the graph DB for a package/bundle directory, mapping a missing index to
/// the same actionable message the CLI path used to surface.
fn open(project_path: &Path) -> Result<GraphDb> {
    let db = crate::codegraph::db_path(project_path);
    if !db.exists() {
        anyhow::bail!(
            "No codegraph index found at '{}'. Run `sempkg index` first.",
            project_path.display()
        );
    }
    GraphDb::open(&db)
        .with_context(|| format!("opening codegraph.db for {}", project_path.display()))
}

/// Serialise a node into the CLI-compatible symbol object (camelCase keys the
/// existing formatters/rerankers read).
fn node_to_json(n: &GraphNode) -> Value {
    json!({
        "id": n.id,
        "name": n.name,
        "qualifiedName": n.qualified_name,
        "kind": n.kind,
        "filePath": n.file_path,
        "language": n.language,
        "startLine": n.start_line,
        "endLine": n.end_line,
        "signature": n.signature,
        "isAsync": n.is_async,
    })
}

/// Wrap a node in the `{ "node": {...} }` envelope the CodeGraph CLI emitted.
/// Every formatter/reranker unwraps via `.get("node")`, and MCP clients (and the
/// functional tests) read `result[i]["node"]["…"]`, so preserving the envelope
/// keeps the response shape identical.
fn envelope(n: &GraphNode) -> Value {
    json!({ "node": node_to_json(n) })
}

fn nodes_to_json(nodes: &[GraphNode]) -> String {
    let arr: Vec<Value> = nodes.iter().map(envelope).collect();
    serde_json::to_string(&arr).unwrap_or_else(|_| "[]".to_string())
}

/// Distinct nodes from a set of call edges, preserving first-seen order. Callers
/// present "who calls X" as distinct symbols, not one row per call site.
fn call_nodes_to_json(edges: &[CallEdge]) -> String {
    let mut seen = std::collections::HashSet::new();
    let arr: Vec<Value> = edges
        .iter()
        .filter(|e| seen.insert(e.node.id.clone()))
        .map(|e| envelope(&e.node))
        .collect();
    serde_json::to_string(&arr).unwrap_or_else(|_| "[]".to_string())
}

/// Search symbols → JSON array (same shape as the old `codegraph query --json`).
pub fn query(
    project_path: &Path,
    search: &str,
    kind: Option<&str>,
    limit: usize,
) -> Result<String> {
    let db = open(project_path)?;
    let nodes = db.query(search, kind, limit)?;
    Ok(nodes_to_json(&nodes))
}

/// Callers of `symbol` → JSON array of distinct caller nodes.
pub fn callers(project_path: &Path, symbol: &str, limit: usize) -> Result<String> {
    let db = open(project_path)?;
    let edges = db.callers(symbol, limit)?;
    Ok(call_nodes_to_json(&edges))
}

/// Callees of `symbol` → JSON array of distinct callee nodes.
pub fn callees(project_path: &Path, symbol: &str, limit: usize) -> Result<String> {
    let db = open(project_path)?;
    let edges = db.callees(symbol, limit)?;
    Ok(call_nodes_to_json(&edges))
}

/// Downstream impact of `symbol` → JSON array of dependent nodes.
pub fn impact(project_path: &Path, symbol: &str, depth: usize) -> Result<String> {
    // The CLI took no explicit cap; bound it generously so a huge blast radius
    // can't return an unbounded payload.
    const IMPACT_LIMIT: usize = 500;
    let db = open(project_path)?;
    let nodes = db.impact(symbol, depth, IMPACT_LIMIT)?;
    Ok(nodes_to_json(&nodes))
}

/// Context for a natural-language `task` → `{ "nodes": [...] }` (the shape the
/// old `codegraph context --format json` produced; `tool_get_context` extracts
/// the `nodes` array and reranks it).
pub fn context(project_path: &Path, task: &str, max_nodes: usize) -> Result<String> {
    let db = open(project_path)?;
    let nodes = db.context(task, max_nodes)?;
    let arr: Vec<Value> = nodes.iter().map(envelope).collect();
    Ok(serde_json::to_string(&json!({ "nodes": arr })).unwrap_or_else(|_| "{\"nodes\":[]}".into()))
}

/// Status of a package/bundle's graph → a compact human-readable block (replaces
/// the CLI's `codegraph status` text on the read path).
pub fn status_text(project_path: &Path) -> Result<String> {
    let db = open(project_path)?;
    let s = db.status()?;
    Ok(format!(
        "CodeGraph index (schema v{}): {} files, {} nodes, {} edges",
        s.schema_version, s.file_count, s.node_count, s.edge_count
    ))
}

/// List files tracked by the index, with optional substring or glob filter and
/// a result cap.
///
/// Matching rules (first that applies wins):
/// 1. If `filter` contains `*` or `?`, it is a glob matched against the full
///    stored path (case-insensitive on Windows).
/// 2. Otherwise it is a case-insensitive substring match.
///
/// Returns a human-readable list, or one of two distinct sentinel messages:
/// - "No files matched …"   → filter valid, zero results.
/// - "Filter error: …"      → glob pattern was syntactically invalid.
pub fn files(project_path: &Path, filter: Option<&str>, limit: usize) -> Result<String> {
    let db = open(project_path)?;
    let all_files = db.file_paths()?;

    let filtered: Vec<&str> = match filter {
        None => all_files.iter().map(String::as_str).collect(),
        Some(pat) => {
            if pat.contains('*') || pat.contains('?') {
                let glob_pat = match glob::Pattern::new(pat) {
                    Ok(p) => p,
                    Err(e) => {
                        return Ok(format!(
                            "Filter error: invalid glob pattern '{pat}': {e}. \
                             Use * for any segment, ** for path wildcards, or a plain substring."
                        ));
                    }
                };
                let opts = glob::MatchOptions {
                    case_sensitive: cfg!(not(target_os = "windows")),
                    require_literal_separator: false,
                    require_literal_leading_dot: false,
                };
                all_files
                    .iter()
                    .filter(|p| glob_pat.matches_with(p, opts))
                    .map(String::as_str)
                    .collect()
            } else {
                let lower = pat.to_lowercase();
                all_files
                    .iter()
                    .filter(|p| p.to_lowercase().contains(&lower))
                    .map(String::as_str)
                    .collect()
            }
        }
    };

    if filtered.is_empty() {
        return Ok(match filter {
            Some(pat) => format!(
                "No files matched filter '{pat}' in package at '{}'. \
                 The index has {} file(s) total. \
                 Try a shorter substring or use * wildcards (e.g. *.rs).",
                project_path.display(),
                all_files.len()
            ),
            None => format!(
                "No files are tracked in the codegraph index at '{}'.",
                project_path.display()
            ),
        });
    }

    let capped = &filtered[..filtered.len().min(limit)];
    let truncated = filtered.len() > capped.len();
    let mut out = capped.join("\n");
    if truncated {
        out.push_str(&format!(
            "\n… {} more file(s) not shown (limit {}). Use a filter to narrow results.",
            filtered.len() - capped.len(),
            limit
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A package-dir layout around the committed fixture: `<dir>/.codegraph/
    /// codegraph.db`. Returns the temp dir (kept alive) and the project path.
    fn fixture_project() -> (tempfile::TempDir, PathBuf) {
        let src =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/codegraph-v4.db");
        assert!(
            src.exists(),
            "required fixture missing at {}",
            src.display()
        );
        let dir = tempfile::TempDir::new().unwrap();
        let cg = dir.path().join(".codegraph");
        std::fs::create_dir_all(&cg).unwrap();
        std::fs::copy(&src, cg.join("codegraph.db")).unwrap();
        let project = dir.path().to_path_buf();
        (dir, project)
    }

    fn parse(s: &str) -> Vec<Value> {
        serde_json::from_str(s).expect("valid JSON array")
    }

    #[test]
    fn query_emits_enveloped_camelcase_symbol_objects() {
        let (_d, p) = fixture_project();
        let arr = parse(&query(&p, "circle_area", None, 10).unwrap());
        // MCP clients read result[i]["node"]["…"] — the envelope must be present.
        let hit = arr
            .iter()
            .find(|v| v["node"]["name"] == "circle_area")
            .expect("circle_area present");
        let node = &hit["node"];
        assert_eq!(node["kind"], "function");
        assert!(node["qualifiedName"].is_string());
        assert!(node["filePath"].as_str().unwrap().contains("shapes.py"));
        assert_eq!(node["startLine"].as_u64().unwrap(), 34);
        assert_eq!(node["endLine"].as_u64().unwrap(), 36);
        assert_eq!(node["signature"], "(radius: Scalar) -> Scalar");
    }

    #[test]
    fn callers_are_distinct_enveloped_nodes() {
        let (_d, p) = fixture_project();
        let arr = parse(&callers(&p, "circle_area", 50).unwrap());
        let names: Vec<&str> = arr
            .iter()
            .filter_map(|v| v["node"]["name"].as_str())
            .collect();
        assert!(names.contains(&"summarize"), "{names:?}");
        // Distinct: no id appears twice.
        let ids: Vec<&str> = arr
            .iter()
            .filter_map(|v| v["node"]["id"].as_str())
            .collect();
        let mut uniq = ids.clone();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(ids.len(), uniq.len(), "duplicate caller nodes: {ids:?}");
    }

    #[test]
    fn impact_and_context_shapes() {
        let (_d, p) = fixture_project();
        let imp = parse(&impact(&p, "circle_area", 3).unwrap());
        assert!(imp.iter().any(|v| v["node"]["name"] == "summarize"));

        let ctx: Value =
            serde_json::from_str(&context(&p, "area of a circle", 20).unwrap()).unwrap();
        let nodes = ctx["nodes"].as_array().expect("nodes array");
        assert!(
            nodes.iter().any(|v| v["node"]["name"] == "circle_area"),
            "{ctx}"
        );
    }

    #[test]
    fn files_listing_and_filter() {
        let (_d, p) = fixture_project();
        let all = files(&p, None, 100).unwrap();
        assert!(all.lines().count() >= 7, "expected 7 files, got:\n{all}");

        let py = files(&p, Some("*.py"), 100).unwrap();
        assert!(py.lines().all(|l| l.ends_with(".py")), "glob leaked: {py}");

        let none = files(&p, Some("no_such_file"), 100).unwrap();
        assert!(none.starts_with("No files matched"), "{none}");
    }

    #[test]
    fn status_text_reports_counts() {
        let (_d, p) = fixture_project();
        let s = status_text(&p).unwrap();
        assert!(s.contains("7 files"), "{s}");
        assert!(s.contains("67 nodes"), "{s}");
        assert!(s.contains("schema v4"), "{s}");
    }

    #[test]
    fn missing_index_is_actionable_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let err = query(dir.path(), "x", None, 5).unwrap_err().to_string();
        assert!(err.contains("No codegraph index found"), "{err}");
    }
}

/// MCP server — JSON-RPC 2.0 over stdio, exposing codegraph + LanceDB tools.
///
/// Protocol: https://spec.modelcontextprotocol.io
/// Transport: stdin/stdout (newline-delimited JSON)
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use std::cell::RefCell;

use crate::packages::PackageRegistry;
use crate::reranker::{self, Reranker, RerankerConfig};
use crate::store::{list_all_bundles, resolve_bundle, resolve_bundle_spec};
use crate::{codegraph, embedding, lance, query_expansion};

use crate::embedding::Embedder;
use crate::query_expansion::{ExpansionKind, QueryExpander};

// Minimum reranker score (P(yes) from the Qwen3 cross-encoder) a result must
// reach to be returned to the agent.  Results below this threshold are
// considered irrelevant and suppressed to avoid wasting context.
// Only applied when the reranker model is actually loaded; the RRF-only
// fallback path does not use this floor (RRF scores carry no absolute
// relevance meaning).
const RERANKER_SCORE_FLOOR: f32 = 0.10;

/// Number of pool hits promoted from cheap pass-1 (snippet reranking) to the
/// expensive pass-2 (small-to-big expansion + KWIC-windowed reranking).
/// Keeping this small (5) means at most 5 × N_windows DB round-trips and
/// reranker calls, regardless of how large the pool is.
const PASS1_K: usize = 5;

/// Maximum characters per KWIC window fed to the pass-2 reranker.
/// At ~4 chars/token this is ≈ 375 tokens, leaving ample room for the
/// Qwen3-0.6B 4096-token context to hold the system prompt + query overhead.
const KWIC_WINDOW_CHARS: usize = 1_500;

/// Stride between consecutive KWIC windows (50 % overlap so that relevant
/// content near a window boundary is always captured in full by at least
/// one window).
const KWIC_STRIDE_CHARS: usize = 750;

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
        tool_schema(
            "query",
            "Unified cross-package search. Submit a natural-language question or keyword query \
             (e.g. 'Where does ADC sampling happen?') and receive ranked results from every \
             installed bundle and local package. Searches code indexes, documentation indexes, \
             and CodeGraph symbol tables across all packages in a single call, then re-ranks \
             all candidates together with the local reranker (when available). \
             Returns rich markdown with package provenance, relevance score, source file, \
             line range, and a source snippet for each hit. \
             Use the other MCP tools (read_code, read_symbol, search_symbols, …) to drill \
             into specific results.",
            json!({
                "query": { "type": "string", "description": "Natural-language or keyword search query" },
                "limit": { "type": "integer", "description": "Max results to return after reranking (default 10)" }
            }),
            &["query"]
        ),
    ])
}

// ---------------------------------------------------------------------------
// Output formatting helpers (shared by all tools)
// ---------------------------------------------------------------------------

/// Format a single codegraph node JSON Value as a compact symbol line.
/// Handles both bare node objects and `{ "node": {...}, "score": N }` envelopes.
fn fmt_codegraph_hit(item: &serde_json::Value, score: Option<f32>) -> String {
    let node = item.get("node").unwrap_or(item);
    let get = |k: &str| node.get(k).and_then(|v| v.as_str()).unwrap_or("");
    let qualified = get("qualifiedName");
    let name = get("name");
    let label = if !qualified.is_empty() {
        qualified
    } else {
        name
    };
    let kind = get("kind");
    let file = get("filePath");
    let sig = get("signature");
    let start = node.get("startLine").and_then(|v| v.as_u64()).unwrap_or(0);
    let end_ln = node.get("endLine").and_then(|v| v.as_u64()).unwrap_or(0);

    let loc = if start > 0 && !file.is_empty() {
        format!("{}:{}-{}", file, start, end_ln)
    } else if !file.is_empty() {
        file.to_string()
    } else {
        String::new()
    };

    let score_str = match score {
        Some(s) => format!("[{:.2}] ", s),
        None => String::new(),
    };

    let header = match (!kind.is_empty(), !loc.is_empty()) {
        (true, true) => format!("{}{} ({}) @ {}", score_str, label, kind, loc),
        (true, false) => format!("{}{} ({})", score_str, label, kind),
        (false, true) => format!("{}{} @ {}", score_str, label, loc),
        _ => format!("{}{}", score_str, label),
    };
    if sig.is_empty() {
        header
    } else {
        format!(
            "{}
{}",
            header, sig
        )
    }
}

/// Parse a codegraph JSON array and render as a compact newline-separated
/// symbol list.  Returns the raw string unchanged when it is not valid JSON.
fn fmt_codegraph_json(json: &str) -> String {
    let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(json) else {
        return json.to_string();
    };
    if arr.is_empty() {
        return "No results.".to_string();
    }
    arr.iter()
        .map(|v| fmt_codegraph_hit(v, None))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Format a single LanceDB `SearchResult` with an optional rerank score.
fn fmt_lance_result(r: &lance::SearchResult, score: Option<f32>) -> String {
    let loc = if r.start_line > 0 {
        format!("{}:{}-{}", r.path, r.start_line, r.end_line)
    } else {
        r.path.clone()
    };
    let score_str = match score {
        Some(s) => format!("[{:.2}] ", s),
        None => String::new(),
    };
    if let Some(sym) = &r.symbol {
        let kind = r.kind.as_deref().unwrap_or("symbol");
        let sig = r.signature.as_deref().unwrap_or("");
        let header = format!("{}{} ({}) @ {}", score_str, sym, kind, loc);
        if sig.is_empty() {
            format!(
                "{}

```
{}
```",
                header, r.snippet
            )
        } else {
            format!(
                "{}
{}

```
{}
```",
                header, sig, r.snippet
            )
        }
    } else {
        format!(
            "{}{}

{}",
            score_str, loc, r.snippet
        )
    }
}

/// Format a slice of LanceDB `SearchResult`s, annotating each with its rerank
/// score when a score map is provided.  Results are separated by `---`.
fn fmt_lance_results(
    results: &[lance::SearchResult],
    score_map: Option<&HashMap<String, f32>>,
) -> String {
    if results.is_empty() {
        return "No results.".to_string();
    }
    results
        .iter()
        .map(|r| {
            let loc_key = if r.start_line > 0 {
                format!("{}:{}-{}", r.path, r.start_line, r.end_line)
            } else {
                r.path.clone()
            };
            let score = score_map.and_then(|m| m.get(&loc_key)).copied();
            fmt_lance_result(r, score)
        })
        .collect::<Vec<_>>()
        .join("\n\n---\n\n")
}

// ---------------------------------------------------------------------------
// Unified query — cross-package hit type
// ---------------------------------------------------------------------------

/// A single search hit collected from any source (code index, docs, codegraph).
struct UnifiedHit {
    /// "pkg@version" or local package name.
    package: String,
    /// "code", "docs", or "codegraph".
    origin: &'static str,
    /// Source file path as recorded in the index.
    path: String,
    /// 1-based start line (0 = unknown).
    start_line: u32,
    /// 1-based end line (0 = unknown).
    end_line: u32,
    /// Text excerpt (snippet, signature, or synthesised description).
    snippet: String,
    /// Symbol name, if applicable.
    symbol: Option<String>,
    /// Symbol kind, if applicable.
    kind: Option<String>,
    /// Symbol signature, if applicable.
    signature: Option<String>,
    /// Reciprocal Rank Fusion score: `1 / (k + rank)` where k=60 and rank is
    /// the 1-based position within this source's own result list.  Applied
    /// uniformly across all origins (code, docs, codegraph) so that no source
    /// can dominate via a different score scale.  Used to sort globally before
    /// the reranker pool is assembled.
    rrf_score: f32,
    /// Full symbol body fetched during the small-to-big expansion pass.
    /// When present this is used as the reranker input instead of the
    /// truncated `snippet`, giving the cross-encoder much richer context.
    /// The `snippet` field is always kept for display output.
    expanded_text: Option<String>,
    /// Best KWIC window from pass-2 reranking.  Populated only for the
    /// top-`PASS1_K` hits that were actually expanded and windowed.
    best_window: Option<String>,
    /// `true` when `best_window` is window 0 (the function-opening /
    /// signature region).  Retained for diagnostics; not used in display
    /// routing — see `format_unified_hit` for rationale.
    #[allow(dead_code)]
    best_window_first: bool,
    /// Total number of KWIC windows the body was split into during pass-2.
    /// 0 = not windowed; 1 = body fits in a single window; >1 = multi-window.
    kwic_count: usize,
}

/// Build the kind+symbol prefix used in all reranker candidate strings.
/// Returns an empty string when neither field is set.
fn hit_kind_symbol_prefix(h: &UnifiedHit) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if let Some(k) = h.kind.as_deref().filter(|s| !s.is_empty()) {
        parts.push(k);
    }
    if let Some(s) = h.symbol.as_deref().filter(|s| !s.is_empty()) {
        parts.push(s);
    }
    parts.join(" ")
}

/// Build the text string submitted to the reranker given an explicit body.
/// This is the inner primitive; callers choose which body to pass:
/// - Pass-1 uses the display snippet.
/// - Pass-2 uses an individual KWIC window from the expanded body.
fn hit_candidate_text_with_body(h: &UnifiedHit, body: &str) -> String {
    let prefix = hit_kind_symbol_prefix(h);
    if prefix.is_empty() {
        body.to_string()
    } else {
        format!("{prefix}: {body}")
    }
}

/// Split `text` into overlapping KWIC windows of at most `KWIC_WINDOW_CHARS`
/// characters with `KWIC_STRIDE_CHARS` stride (≈ 50 % overlap).
///
/// All splits are snapped to UTF-8 char boundaries.  If `text` fits in a
/// single window the returned `Vec` has exactly one element (no windowing
/// overhead, same as before).
fn kwic_windows(text: &str) -> Vec<String> {
    if text.len() <= KWIC_WINDOW_CHARS {
        return vec![text.to_string()];
    }
    let mut windows: Vec<String> = Vec::new();
    let mut start = 0usize;
    loop {
        // Snap end forward to the next newline so windows never cut mid-line.
        let raw_end = (start + KWIC_WINDOW_CHARS).min(text.len());
        let end = if raw_end == text.len() {
            text.len()
        } else {
            // Find the next '\n' at or after raw_end; fall back to text.len().
            text[raw_end..]
                .find('\n')
                .map(|rel| raw_end + rel + 1) // include the '\n'
                .unwrap_or(text.len())
        };
        windows.push(text[start..end].to_string());
        if end == text.len() {
            break;
        }
        // Advance stride, snapping forward to the next newline boundary.
        let raw_next = start + KWIC_STRIDE_CHARS;
        let next = if raw_next >= text.len() {
            text.len()
        } else {
            text[raw_next..]
                .find('\n')
                .map(|rel| raw_next + rel + 1)
                .unwrap_or(text.len())
        };
        if next <= start {
            break; // safety: never loop if stride maps to zero advance
        }
        start = next;
    }
    windows
}

/// Decide whether the query matched lexically in the symbol's name or
/// signature, as opposed to only in the body.  Drives the tiered display:
/// a name/signature match can be shown as signature-only (the heading + the
/// `Signature:` line already carry the matched text), whereas a body match
/// must show the relevant code window.
///
/// The match is attribution-based, not size-based: a one-window body that the
/// query hit *in the body* still shows the window, while a large body whose
/// name the query hit can collapse to the signature.
fn query_matches_name_or_signature(query: &str, h: &UnifiedHit) -> bool {
    let mut haystack = String::new();
    if let Some(s) = h.symbol.as_deref() {
        haystack.push_str(s);
        haystack.push(' ');
    }
    if let Some(s) = h.signature.as_deref() {
        haystack.push_str(s);
    }
    let haystack = haystack.to_lowercase();
    if haystack.trim().is_empty() {
        return false;
    }

    // Meaningful query terms: alphanumeric tokens of length >= 3 (drops "the",
    // "of", "to", and punctuation).
    let terms: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 3)
        .map(|t| t.to_lowercase())
        .collect();
    if terms.is_empty() {
        return false;
    }

    // Name match when at least half of the meaningful terms appear in the
    // name/signature.  Requiring a majority avoids treating an incidental
    // single-term overlap as a full name match.
    let matched = terms
        .iter()
        .filter(|t| haystack.contains(t.as_str()))
        .count();
    matched * 2 >= terms.len()
}

/// Format a `UnifiedHit` as a markdown section for the `query` tool output.
fn format_unified_hit(h: &UnifiedHit, query: &str, rank: usize, score: Option<f32>) -> String {
    // ── Header line ──────────────────────────────────────────────────────
    let score_str = match score {
        Some(s) => format!(" · score {s:.3}"),
        None => String::new(),
    };
    let label = h
        .symbol
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(&h.path);
    let kind_str = h
        .kind
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|k| format!(" ({k})"))
        .unwrap_or_default();

    let header = format!("### {rank}. `{label}`{kind_str}{score_str}");

    // ── Metadata table ───────────────────────────────────────────────────
    let lines_str = if h.start_line > 0 && h.end_line > 0 {
        format!("{} – {}", h.start_line, h.end_line)
    } else if h.start_line > 0 {
        h.start_line.to_string()
    } else {
        "—".to_string()
    };

    let meta = format!(
        "| Field | Value |\n\
         |-------|-------|\n\
         | **Package** | `{package}` |\n\
         | **Origin** | {origin} |\n\
         | **Source** | `{path}` |\n\
         | **Lines** | {lines} |",
        package = h.package,
        origin = h.origin,
        path = h.path,
        lines = lines_str,
    );

    // ── Signature (if present) ───────────────────────────────────────────
    let sig_block = h
        .signature
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|s| format!("\n**Signature:** `{s}`"))
        .unwrap_or_default();

    // ── Snippet block (tiered display) ───────────────────────────────────
    // Tier is chosen by *where the query matched*, not by body size:
    //
    // 1. Name/signature match (`query_matches_name_or_signature`) whose best
    //    window is the opening region: the heading + `Signature:` line already
    //    show the matched text, so the code block is omitted as redundant.
    //
    // 2. Body match (query did not hit the name/signature, or the best window
    //    is deeper in the body): show the best KWIC window with a
    //    "+N more lines → read_code" affordance.  This fires even for
    //    single-window bodies — a one-window body matched in its body still
    //    deserves to show that body.
    //
    // 3. No pass-2 window available (no reranker / fallback path): show the
    //    original display snippet verbatim.
    let snippet_block = if let Some(ref window) = h.best_window {
        // Tier 1: query terms land in the name/signature — the heading +
        // Signature line already show the matched text, no code block needed.
        // `best_window_first` is intentionally NOT part of this check: for
        // functions whose body fits in a single KWIC window (< 1500 chars)
        // `best_window_first` is trivially true and would incorrectly suppress
        // the body for every small function regardless of where the match was.
        if query_matches_name_or_signature(query, h) {
            String::new()
        } else {
            // Tier 2: show the best KWIC window + optional affordance.
            let affordance = if h.start_line > 0 && h.end_line > h.start_line {
                let total_lines = (h.end_line - h.start_line + 1) as usize;
                let window_lines = window.lines().count();
                // A multi-window body always has unseen content; otherwise fall
                // back to the line-range estimate.
                if h.kwic_count > 1 || total_lines > window_lines + 2 {
                    let more = total_lines.saturating_sub(window_lines);
                    format!(
                        "\n*+{more} more lines — call `read_code` with \
                         package `{}`, file `{}`, line `{}`*",
                        h.package, h.path, h.start_line
                    )
                } else {
                    String::new()
                }
            } else {
                String::new()
            };
            format!("\n```\n{window}\n```{affordance}")
        }
    } else if h.snippet.is_empty() {
        String::new()
    } else {
        format!("\n```\n{}\n```", h.snippet)
    };

    format!("{header}\n\n{meta}{sig_block}{snippet_block}")
}

// ---------------------------------------------------------------------------
// Dedup helpers for tool_query
// ---------------------------------------------------------------------------

/// Normalise a file path to a canonical dedup key component:
/// lowercase + forward-slash separators so that Windows codegraph paths
/// (`src\adc.rs`) and lance paths (`src/adc.rs`) match.
fn normalise_path(path: &str) -> String {
    path.replace('\\', "/").to_lowercase()
}

/// Build the dedup key for a `UnifiedHit`.
///
/// - Code / codegraph hits sharing the same file and start line represent the
///   same symbol and are collapsed regardless of origin.
/// - Doc hits are keyed by file + start + end so that distinct chunks from the
///   same document are preserved while truly identical chunks are merged.
/// - When line information is unavailable the symbol name is used as a
///   tiebreaker to avoid over-aggressive collapsing.
fn dedup_key(h: &UnifiedHit) -> String {
    let path = normalise_path(&h.path);
    if h.origin == "docs" {
        // Doc chunks carry no line numbers (both fields are always 0), so line
        // range cannot distinguish chunks.  Hash the snippet content instead:
        // identical chunks collapse; distinct chunks of the same document are
        // preserved.  Package is prefixed so the same doc in two bundles stays
        // separate and both contribute to cross-package RRF.
        let mut hasher = DefaultHasher::new();
        h.snippet.hash(&mut hasher);
        let hash = hasher.finish();
        return format!("{}:{}:{:x}", h.package, path, hash);
    }
    // For code / codegraph: package + normalised path + start line is a
    // precise per-symbol key that matches Windows and Unix path variants.
    if h.start_line > 0 {
        format!("{}:{}:{}", h.package, path, h.start_line)
    } else if let Some(sym) = h.symbol.as_deref().filter(|s| !s.is_empty()) {
        format!("{}:{}:{}", h.package, path, sym.to_lowercase())
    } else {
        format!("{}:{}", h.package, path)
    }
}

/// Richness rank: higher = more payload.  `code` carries the source body and
/// wins over `codegraph` (structured location only) which wins over `docs`.
fn origin_priority(origin: &str) -> u8 {
    match origin {
        "code" => 2,
        "codegraph" => 1,
        _ => 0,
    }
}

/// Merge complementary structured fields from `donor` into `winner` in-place.
///
/// After a collision the winner already holds the richer snippet/body.  This
/// pass fills in any fields the winner is missing — typically the qualified
/// symbol name from a codegraph hit when the code-index hit only recorded the
/// short name.
fn merge_complementary(winner: &mut UnifiedHit, donor: &UnifiedHit) {
    // Prefer the longer (more qualified) symbol name.
    match (&winner.symbol, &donor.symbol) {
        (Some(w), Some(d)) if d.len() > w.len() => winner.symbol = Some(d.clone()),
        (None, Some(d)) => winner.symbol = Some(d.clone()),
        _ => {}
    }
    // Fill missing signature / kind from donor.
    if winner.signature.as_deref().unwrap_or("").is_empty() {
        if let Some(s) = donor.signature.as_deref().filter(|s| !s.is_empty()) {
            winner.signature = Some(s.to_string());
        }
    }
    if winner.kind.as_deref().unwrap_or("").is_empty() {
        if let Some(k) = donor.kind.as_deref().filter(|s| !s.is_empty()) {
            winner.kind = Some(k.to_string());
        }
    }
    // Prefer accurate line range (non-zero wins).
    if winner.start_line == 0 && donor.start_line > 0 {
        winner.start_line = donor.start_line;
        winner.end_line = donor.end_line;
    }
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
    /// Optionally-loaded vector embedder for semantic search.
    embedder: Option<Embedder>,
    /// Identifier of the loaded embedding model (for bundle compatibility checks).
    embed_model_id: Option<String>,
    /// Optionally-loaded generative query expander.
    expander: Option<QueryExpander>,
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

        // Embedding config + lazy model load (semantic search).
        let embed_cfg: embedding::EmbeddingConfig = workspace_dir
            .as_deref()
            .and_then(|d| crate::manifest::load_manifest(d).ok())
            .and_then(|mf| mf.embedding)
            .unwrap_or_default();

        let (embedder, embed_model_id) =
            if embed_cfg.enabled && embedding::model_is_present(&embed_cfg) {
                match Embedder::load(&embed_cfg) {
                    Ok(e) => {
                        eprintln!(
                            "sempkg: embedder loaded ({}, dim {})",
                            embedding::EMBED_MODEL_ID,
                            e.dim()
                        );
                        (Some(e), Some(embedding::EMBED_MODEL_ID.to_string()))
                    }
                    Err(e) => {
                        eprintln!("sempkg: embedder load error (vector search disabled): {e}");
                        (None, None)
                    }
                }
            } else {
                (None, None)
            };

        // Query-expansion config + lazy model load.
        let qe_cfg: query_expansion::QueryExpansionConfig = workspace_dir
            .as_deref()
            .and_then(|d| crate::manifest::load_manifest(d).ok())
            .and_then(|mf| mf.query_expansion)
            .unwrap_or_default();

        let expander = if qe_cfg.enabled && query_expansion::model_is_present(&qe_cfg) {
            match QueryExpander::load(&qe_cfg) {
                Ok(e) => {
                    eprintln!("sempkg: query expander loaded");
                    Some(e)
                }
                Err(e) => {
                    eprintln!("sempkg: query expander load error (expansion disabled): {e}");
                    None
                }
            }
        } else {
            None
        };

        Self {
            workspace_dir,
            registry,
            reranker: RefCell::new(reranker),
            reranker_cfg,
            embedder,
            embed_model_id,
            expander,
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

                self.apply_rerank_to_codegraph_json(query, &raw, limit)
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
                return codegraph::context(&path, task).unwrap_or_else(|e| format!("Error: {e}"));
            }
        };

        // Parse the JSON response: extract the `nodes` array and re-serialise
        // it as a plain array so `codegraph_json_to_candidates` can consume it.
        let nodes_json: String = match serde_json::from_str::<serde_json::Value>(&raw) {
            Ok(v) => {
                let nodes = v
                    .get("nodes")
                    .cloned()
                    .unwrap_or(serde_json::Value::Array(vec![]));
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
                self.fmt_codegraph_with_source(package, &raw)
            }
        }
    }

    fn tool_get_callees(&self, package: &str, symbol: &str, limit: usize) -> String {
        match self.resolve_codegraph_path(package) {
            Err(e) => e,
            Ok(path) => {
                let raw = codegraph::callees(&path, symbol, limit)
                    .unwrap_or_else(|e| format!("Error: {e}"));
                self.fmt_codegraph_with_source(package, &raw)
            }
        }
    }

    fn tool_get_impact(&self, package: &str, symbol: &str, depth: usize) -> String {
        match self.resolve_codegraph_path(package) {
            Err(e) => e,
            Ok(path) => {
                let raw = codegraph::impact(&path, symbol, depth)
                    .unwrap_or_else(|e| format!("Error: {e}"));
                fmt_codegraph_json(&raw)
            }
        }
    }

    fn tool_list_files(&self, package: &str, filter: Option<&str>, limit: usize) -> String {
        match self.resolve_codegraph_path(package) {
            Err(e) => e,
            Ok(path) => {
                codegraph::files(&path, filter, limit).unwrap_or_else(|e| format!("Error: {e}"))
            }
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

    fn tool_search_code(
        &self,
        package: &str,
        query: &str,
        kind_filter: Option<&str>,
        limit: usize,
    ) -> String {
        match self.resolve_code_path(package) {
            Err(e) => e,
            Ok(code_dir) => {
                let fetch_limit = self.reranker_fetch_limit(limit);
                match lance::search_code(&code_dir, query, fetch_limit) {
                    Err(e) => format!("Error searching code: {e}"),
                    Ok(mut results) => {
                        // Client-side kind filter
                        if let Some(k) = kind_filter {
                            results.retain(|r| r.kind.as_deref().map_or(false, |rk| rk == k));
                        }
                        self.apply_rerank_to_lance(query, results, limit)
                    }
                }
            }
        }
    }

    /// Small-to-big expansion: for each hit in `pool_indices`, attempt to
    /// replace the truncated display snippet with the full symbol body from
    /// the code index, giving the reranker far richer context to score.
    ///
    /// Strategy (in priority order):
    /// 1. `code` origin hits: fetch by `path + start_line` (precise location).
    /// 2. `codegraph` origin hits: try the same location lookup via the code
    ///    index; fall back to symbol-name lookup when line data is missing.
    /// 3. `docs` origin hits: skipped — doc chunks are already at their natural
    ///    granularity and have no separate code-index entry to expand into.
    ///
    /// On any lookup failure the hit is left unchanged (snippet still used).
    /// `expanded_text` is set only on hits where the returned content is
    /// strictly longer than the existing snippet, so truncated snippets are
    /// always replaced by something richer.
    fn expand_pool_hits(&self, hits: &mut Vec<UnifiedHit>, pool_indices: &[usize]) {
        let debug = std::env::var("SEMPKG_DEBUG").is_ok();
        let mut attempted = 0usize;
        let mut resolved = 0usize;
        let mut expanded = 0usize;
        for &hit_idx in pool_indices {
            // Extract the fields we need before taking any mutable reference.
            let (origin, pkg, path, start_line, symbol) = {
                let h = &hits[hit_idx];
                (
                    h.origin,
                    h.package.clone(),
                    h.path.clone(),
                    h.start_line,
                    h.symbol.clone(),
                )
            };

            if origin == "docs" {
                continue;
            }
            attempted += 1;

            let code_dir = match self.resolve_code_path(&pkg) {
                Ok(d) => d,
                Err(_) => continue, // no code index for this package
            };
            resolved += 1;

            // Primary: location-keyed lookup (path + start_line).
            let mut full_body: Option<String> = if start_line > 0 {
                lance::fetch_symbol_at_location(&code_dir, &path, start_line)
                    .ok()
                    .flatten()
                    .map(|src| src.content)
            } else {
                None
            };

            // Fallback: symbol-name lookup (for codegraph hits without line info).
            if full_body.is_none() {
                if let Some(sym) = symbol.as_deref().filter(|s| !s.is_empty()) {
                    if let Ok(lance::SymbolLookup::Unique(src)) =
                        lance::fetch_symbol_source(&code_dir, sym)
                    {
                        full_body = Some(src.content);
                    }
                }
            }

            // Only store when we actually got something longer than the snippet.
            if let Some(body) = full_body {
                let current_len = hits[hit_idx].snippet.len();
                if body.len() > current_len {
                    hits[hit_idx].expanded_text = Some(body);
                    expanded += 1;
                }
            }
        }
        if debug {
            eprintln!(
                "sempkg: expand_pool_hits — pool={} expandable={} code_dir_resolved={} expanded={}",
                pool_indices.len(),
                attempted,
                resolved,
                expanded
            );
        }
    }

    /// Unified cross-package search: queries code, docs, and codegraph across
    /// all installed bundles and local packages, then reranks everything together.
    /// Retrieve BM25 (+ optional vector) hits for a single (possibly expanded)
    /// query, scaling each hit's RRF contribution by `weight`. Lexical sources
    /// (BM25 + codegraph) run when `do_lex`; vector search runs when `do_vec`
    /// and a compatible embedder + stored vectors are available.
    ///
    /// Hits are appended to `hits`; the caller fuses duplicates across runs by
    /// summing their RRF scores (Reciprocal Rank Fusion).
    #[allow(clippy::too_many_arguments)]
    fn collect_query_hits(
        &self,
        query: &str,
        weight: f32,
        do_lex: bool,
        do_vec: bool,
        fetch_limit: usize,
        hits: &mut Vec<UnifiedHit>,
    ) {
        // Embed the query once (reused across every table) when vector search
        // is requested and an embedder is loaded. `None` => vector search is
        // silently skipped and the run degrades to BM25 only.
        let query_vec: Option<Vec<f32>> = if do_vec {
            match (self.embedder.as_ref(), self.embed_model_id.as_ref()) {
                (Some(e), Some(_)) => e.embed_query(query).ok(),
                _ => None,
            }
        } else {
            None
        };

        // Whether `dir`'s table was embedded with the model we'd query with.
        let vectors_compatible = |dir: &Path, qlen: usize| -> bool {
            match lance::read_embedding_info(dir) {
                Some((model, dim)) => {
                    self.embed_model_id.as_deref() == Some(model.as_str()) && dim as usize == qlen
                }
                None => false,
            }
        };

        // ── Helper: push lance SearchResults with weighted RRF ───────────
        let push_results = |hits: &mut Vec<UnifiedHit>,
                            pkg: &str,
                            origin: &'static str,
                            rs: Vec<lance::SearchResult>| {
            for (pos, r) in rs.into_iter().enumerate() {
                hits.push(UnifiedHit {
                    package: pkg.to_string(),
                    origin,
                    path: r.path,
                    start_line: r.start_line,
                    end_line: r.end_line,
                    snippet: r.snippet,
                    symbol: r.symbol,
                    kind: r.kind,
                    signature: r.signature,
                    rrf_score: weight / (60.0 + pos as f32 + 1.0),
                    expanded_text: None,
                    best_window: None,
                    best_window_first: false,
                    kwic_count: 0,
                });
            }
        };

        // ── Helper: push codegraph JSON nodes into hits ──────────────────
        let push_codegraph_hits = |hits: &mut Vec<UnifiedHit>, pkg_name: &str, raw_json: &str| {
            let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(raw_json) else {
                return;
            };
            for (pos, v) in arr.iter().enumerate() {
                let node = v.get("node").unwrap_or(v);
                let get_str = |k: &str| {
                    node.get(k)
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string()
                };
                let qualified = get_str("qualifiedName");
                let name = get_str("name");
                let label = if !qualified.is_empty() {
                    qualified
                } else {
                    name
                };
                let kind = get_str("kind");
                let sig = get_str("signature");
                let file = get_str("filePath");
                let start = node.get("startLine").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
                let end = node.get("endLine").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
                let snippet = if !sig.is_empty() {
                    sig.clone()
                } else {
                    format!("{kind} {label}")
                };
                // Uniform RRF — do NOT use the codegraph envelope score here.
                // Codegraph BM25 and lance BM25 are on different scales; mixing
                // them biases the global sort.  Rank position is the only
                // comparable signal across heterogeneous sources.
                let rrf_score = weight / (60.0 + pos as f32 + 1.0);
                hits.push(UnifiedHit {
                    package: pkg_name.to_string(),
                    origin: "codegraph",
                    path: file,
                    start_line: start,
                    end_line: end,
                    snippet,
                    symbol: Some(label),
                    kind: Some(kind),
                    signature: Some(sig),
                    rrf_score,
                    expanded_text: None,
                    best_window: None,
                    best_window_first: false,
                    kwic_count: 0,
                });
            }
        };

        // ── Installed bundles ────────────────────────────────────────────
        let bundles = list_all_bundles(self.workspace().map(|p| p.as_path()));
        for bundle in &bundles {
            let pkg_name = format!("{}@{}", bundle.name, bundle.version);

            // Source-code index
            if bundle.has_code() {
                let code_dir = bundle.bundle_dir.join("code");
                if do_lex {
                    if let Ok(results) = lance::search_code(&code_dir, query, fetch_limit) {
                        push_results(hits, &pkg_name, "code", results);
                    }
                }
                if let Some(qv) = &query_vec {
                    if vectors_compatible(&code_dir, qv.len()) {
                        if let Ok(results) = lance::search_code_vector(&code_dir, qv, fetch_limit) {
                            push_results(hits, &pkg_name, "code", results);
                        }
                    }
                }
            }

            // Documentation index
            if bundle.has_lance() {
                let lance_dir = bundle.bundle_dir.join("lance");
                if do_lex {
                    if let Ok(results) = lance::search(&lance_dir, query, fetch_limit) {
                        push_results(hits, &pkg_name, "docs", results);
                    }
                }
                if let Some(qv) = &query_vec {
                    if vectors_compatible(&lance_dir, qv.len()) {
                        if let Ok(results) = lance::search_vector(&lance_dir, qv, fetch_limit) {
                            push_results(hits, &pkg_name, "docs", results);
                        }
                    }
                }
            }

            // CodeGraph symbol search (lexical only).
            if do_lex && bundle.is_indexed() {
                if let Ok(raw) = codegraph::query(&bundle.bundle_dir, query, None, fetch_limit) {
                    push_codegraph_hits(hits, &pkg_name, &raw);
                }
            }
        }

        // ── Local packages ───────────────────────────────────────────────
        for pkg in self.registry.list() {
            let pkg_name = pkg.name.clone();

            // Lance docs index at <pkg>/.sempkg/lance/
            let lance_dir = pkg.abs_path().join(".sempkg").join("lance");
            if lance_dir.is_dir() {
                if do_lex {
                    if let Ok(results) = lance::search(&lance_dir, query, fetch_limit) {
                        push_results(hits, &pkg_name, "docs", results);
                    }
                }
                if let Some(qv) = &query_vec {
                    if vectors_compatible(&lance_dir, qv.len()) {
                        if let Ok(results) = lance::search_vector(&lance_dir, qv, fetch_limit) {
                            push_results(hits, &pkg_name, "docs", results);
                        }
                    }
                }
            }

            // CodeGraph (lexical only).
            if do_lex && pkg.is_indexed() {
                if let Ok(raw) = codegraph::query(&pkg.abs_path(), query, None, fetch_limit) {
                    push_codegraph_hits(hits, &pkg_name, &raw);
                }
            }
        }
    }

    fn tool_query(&self, query: &str, limit: usize) -> String {
        let fetch_limit = self.reranker_fetch_limit(limit).max(20);
        let mut hits: Vec<UnifiedHit> = Vec::new();

        // ── Query expansion → retrieval runs ─────────────────────────────
        // The original query always runs against BOTH backends with double
        // weight (it is the most trustworthy signal — matches QMD). Expanded
        // variants are routed by type: `lex` → BM25, `vec`/`hyde` → vector.
        // When no expander is loaded (model missing / feature off), the single
        // original run still drives full BM25 + vector search.
        let mut runs: Vec<(String, f32, bool, bool)> = vec![(query.to_string(), 2.0, true, true)];

        if let Some(expander) = self.expander.as_ref() {
            for variant in expander.expand(query) {
                match variant.kind {
                    ExpansionKind::Lexical => runs.push((variant.text, 1.0, true, false)),
                    ExpansionKind::Vector => runs.push((variant.text, 1.0, false, true)),
                }
            }
        }

        for (q, weight, do_lex, do_vec) in &runs {
            self.collect_query_hits(q, *weight, *do_lex, *do_vec, fetch_limit, &mut hits);
        }

        if hits.is_empty() {
            return format!(
                "No results found for: `{query}`\n\n\
                 Make sure at least one package is indexed or a bundle with a code/docs \
                 index is installed (`sempkg list`)."
            );
        }

        // ── Dedup ────────────────────────────────────────────────────────
        // O(n) pass: collapse codegraph and code-index hits that refer to the
        // same symbol at the same file:line.  On collision the richer origin
        // wins (code > codegraph > docs); structured fields absent from the
        // winner are filled in from the loser so no location or signature data
        // is lost.
        let mut hits: Vec<UnifiedHit> = {
            let mut key_map: HashMap<String, usize> = HashMap::with_capacity(hits.len());
            let mut deduped: Vec<UnifiedHit> = Vec::with_capacity(hits.len());
            for hit in hits {
                let key = dedup_key(&hit);
                if let Some(&idx) = key_map.get(&key) {
                    let existing_priority = origin_priority(deduped[idx].origin);
                    let new_priority = origin_priority(hit.origin);
                    let should_replace = new_priority > existing_priority
                        || (new_priority == existing_priority
                            && hit.snippet.len() > deduped[idx].snippet.len());
                    if should_replace {
                        let old = std::mem::replace(&mut deduped[idx], hit);
                        merge_complementary(&mut deduped[idx], &old);
                        // RRF fusion: a hit found by multiple runs/sources
                        // accumulates their reciprocal-rank contributions.
                        deduped[idx].rrf_score += old.rrf_score;
                    } else {
                        // Existing wins; harvest complementary fields and add
                        // this run's RRF contribution to the running total.
                        let new_rrf = hit.rrf_score;
                        merge_complementary(&mut deduped[idx], &hit);
                        deduped[idx].rrf_score += new_rrf;
                    }
                } else {
                    key_map.insert(key, deduped.len());
                    deduped.push(hit);
                }
            }
            deduped
        };

        // ── Global RRF sort ───────────────────────────────────────────────
        // All sources used the same RRF formula so this sort is apples-to-apples.
        // Rank-1 from every (package, origin) pair competes fairly.
        hits.sort_by(|a, b| {
            b.rrf_score
                .partial_cmp(&a.rrf_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // ── Diversity selection ───────────────────────────────────────────
        // Greedy pass over the RRF-sorted list: accept each hit until its
        // (package, origin) bucket is full, stopping once the pool has
        // `pool_size` items.  This prevents any single source (e.g. codegraph
        // from one large package) from monopolising the reranker's top_k slots.
        //
        // max_per_bucket ≈ pool_size / 3 so the three origins (code, docs,
        // codegraph) each get roughly equal representation.
        let pool_size = fetch_limit;
        let max_per_bucket = (pool_size / 3).max(3);
        let mut bucket_counts: HashMap<(String, &'static str), usize> = HashMap::new();
        let pool_indices: Vec<usize> = hits
            .iter()
            .enumerate()
            .filter_map(|(i, h)| {
                let count = bucket_counts
                    .entry((h.package.clone(), h.origin))
                    .or_insert(0);
                if *count < max_per_bucket {
                    *count += 1;
                    Some(i)
                } else {
                    None
                }
            })
            .take(pool_size)
            .collect();

        // ── Small-to-big expansion ────────────────────────────────────────
        // Fetch the full symbol body for each code/codegraph hit in the pool.
        // The reranker then scores the complete implementation + its comment
        // context rather than the truncated 600-char display snippet.
        // Doc hits are left unchanged (their chunks are already the natural
        // retrieval unit).
        self.expand_pool_hits(&mut hits, &pool_indices);

        // ── Two-pass reranking ────────────────────────────────────────────
        //
        // Pass 1 (cheap): score every pool hit using its display snippet.
        //   The snippets are already in memory and small enough that the
        //   reranker can process the whole pool in one call.  Output is a
        //   ranked list used both to choose pass-2 candidates and to order any
        //   tail beyond the pass-2 budget.
        //
        // Pass 2 (expensive): for each promoted hit:
        //   1. Use the expanded full body (from small-to-big) if available.
        //   2. Split the body into overlapping KWIC windows.
        //   3. Score every (query, window) pair and take the max.
        //   4. Record which window scored best for tiered display.
        //
        // The pass-2 budget tracks the output `limit` (with PASS1_K as a floor)
        // so the result count is never capped at PASS1_K.  Hits beyond the
        // budget keep their cheap pass-1 score and are appended as a tail.
        let mut reranker_was_active = false;
        let scored: Vec<(usize, f32)> = {
            let mut guard = self.reranker.borrow_mut();
            if let Some(ranker) = guard.as_mut() {
                // ── Pass 1: snippet scoring ───────────────────────────────
                let p1_candidates: Vec<reranker::RerankCandidate> = pool_indices
                    .iter()
                    .enumerate()
                    .map(|(pp, &hi)| {
                        let sig = hits[hi].signature.as_deref().unwrap_or("");
                        let body = if !hits[hi].snippet.is_empty() {
                            hits[hi].snippet.as_str()
                        } else if !sig.is_empty() {
                            sig
                        } else {
                            hits[hi].path.as_str()
                        };
                        reranker::RerankCandidate {
                            source: pp.to_string(),
                            text: hit_candidate_text_with_body(&hits[hi], body),
                            origin: reranker::RerankOrigin::Docs,
                        }
                    })
                    .collect();

                // Score every candidate with score_pair() directly — no
                // top_k or output_n truncation, which rerank() applies
                // internally and would cap p1_scored to output_n (= 5)
                // regardless of how large pass2_budget or limit are.
                let mut p1_scored: Vec<(usize, f32)> = p1_candidates
                    .iter()
                    .enumerate()
                    .map(|(pp, c)| {
                        let score = ranker.score_pair(query, &c.text).unwrap_or(0.0);
                        let hi = pool_indices[pp];
                        (hi, score)
                    })
                    .collect();
                p1_scored
                    .sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

                // Promote the top `pass2_budget` hits to the expensive pass.
                // Budget tracks `limit` so output is not capped at PASS1_K;
                // PASS1_K is only a floor (small limits still expand a few
                // candidates so the best of them can be picked accurately).
                let pass2_budget = limit.max(PASS1_K);
                let top_indices: Vec<usize> = p1_scored
                    .iter()
                    .take(pass2_budget)
                    .map(|&(i, _)| i)
                    .collect();

                // ── Pass 2: KWIC-windowed scoring on expanded bodies ──────
                // Call score_pair() directly per window instead of going through
                // rerank(), which would silently truncate via its top_k cap
                // (applied before scoring) and output_n cap (applied after) —
                // both would drop valid windows when total windows > top_k.
                //
                // By calling score_pair() ourselves we score every window of
                // every promoted hit, regardless of how many windows a large
                // body produces, and accumulate the maximum per hit without
                // losing any candidates to internal caps.
                reranker_was_active = true;
                let mut best: HashMap<usize, (f32, usize)> = HashMap::new();

                for (tp, &hi) in top_indices.iter().enumerate() {
                    let body = {
                        let h = &hits[hi];
                        h.expanded_text
                            .as_deref()
                            .filter(|s| !s.is_empty())
                            .unwrap_or(if !h.snippet.is_empty() {
                                h.snippet.as_str()
                            } else {
                                h.path.as_str()
                            })
                            .to_string()
                    };
                    let windows = kwic_windows(&body);
                    for (wi, window) in windows.iter().enumerate() {
                        let text = hit_candidate_text_with_body(&hits[hi], window);
                        let score = ranker.score_pair(query, &text).unwrap_or(0.0);
                        let e = best.entry(tp).or_insert((f32::NEG_INFINITY, 0));
                        if score > e.0 {
                            *e = (score, wi);
                        }
                    }
                }

                // Write best_window / best_window_first / kwic_count back into
                // each hit.  Body + windows are recomputed cheaply from the
                // already-fetched expanded_text — no second DB round-trip.
                for (tp, &hi) in top_indices.iter().enumerate() {
                    if let Some(&(_, wi)) = best.get(&tp) {
                        let body = {
                            let h = &hits[hi];
                            h.expanded_text
                                .as_deref()
                                .filter(|s| !s.is_empty())
                                .unwrap_or(if !h.snippet.is_empty() {
                                    h.snippet.as_str()
                                } else {
                                    h.path.as_str()
                                })
                                .to_string()
                        };
                        let windows = kwic_windows(&body);
                        let n = windows.len();
                        hits[hi].best_window = windows.into_iter().nth(wi);
                        hits[hi].best_window_first = wi == 0;
                        hits[hi].kwic_count = n;
                    }
                }

                // Final ranking: promoted hits sorted by their pass-2 score…
                let mut final_scored: Vec<(usize, f32)> = top_indices
                    .iter()
                    .enumerate()
                    .filter_map(|(tp, &hi)| best.get(&tp).map(|&(s, _)| (hi, s)))
                    .collect();
                final_scored
                    .sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                // …then the cheap pass-1 tail (hits beyond the budget), in
                // pass-1 order, appended after the expanded set.  This backfills
                // the output if expanded hits are later dropped by the floor.
                final_scored.extend(p1_scored.iter().skip(pass2_budget).copied());
                final_scored.into_iter().take(limit).collect()
            } else {
                // No reranker — return top N from the diversity-selected pool, scored by RRF.
                pool_indices
                    .iter()
                    .take(limit)
                    .map(|&i| (i, hits[i].rrf_score))
                    .collect()
            }
        };

        // ── Relevance floor ──────────────────────────────────────────────
        // Drop results the reranker considers irrelevant.  Only applied when
        // the reranker ran; RRF scores are not a relevance signal.
        let scored: Vec<(usize, f32)> = if reranker_was_active {
            scored
                .into_iter()
                .filter(|&(_, s)| s >= RERANKER_SCORE_FLOOR)
                .collect()
        } else {
            scored
        };

        if scored.is_empty() {
            return if reranker_was_active {
                format!(
                    "No relevant results for: `{query}`\n\n\
                     All candidates scored below the relevance floor ({RERANKER_SCORE_FLOOR:.2}). \
                     Try rephrasing the query or use `search_code`, `search_docs`, or \
                     `search_symbols` to inspect a specific package directly."
                )
            } else {
                format!("No results for: `{query}`.")
            };
        }

        // ── Format ───────────────────────────────────────────────────────
        let has_scores = scored.iter().any(|(_, s)| *s > 0.0);
        let sections: Vec<String> = scored
            .iter()
            .enumerate()
            .map(|(rank, &(idx, score))| {
                format_unified_hit(
                    &hits[idx],
                    query,
                    rank + 1,
                    if has_scores { Some(score) } else { None },
                )
            })
            .collect();

        format!(
            "## Query results for: `{query}`\n\n{}\n",
            sections.join("\n\n---\n\n")
        )
    }

    fn tool_read_code(&self, package: &str, file: &str, line: u32) -> String {
        match self.resolve_code_path(package) {
            Err(e) => e,
            Ok(code_dir) => match lance::fetch_symbol_at_location(&code_dir, file, line) {
                Err(e) => format!("Error reading code: {e}"),
                Ok(None) => format!(
                    "No symbol found covering {file}:{line} in the code index for '{package}'. \
                         Verify the file path and line number from the codegraph results, or use \
                         read_symbol to look up by name."
                ),
                Ok(Some(src)) => {
                    let loc = format!("{}:{}-{}", src.path, src.start_line, src.end_line);
                    if src.signature.is_empty() {
                        format!(
                            "**{}** ({}) @ {}\n\n```\n{}\n```",
                            src.symbol, src.kind, loc, src.content
                        )
                    } else {
                        format!(
                            "**{}** ({}) @ {}\n{}\n\n```\n{}\n```",
                            src.symbol, src.kind, loc, src.signature, src.content
                        )
                    }
                }
            },
        }
    }

    fn tool_read_symbol(&self, package: &str, symbol: &str) -> String {
        match self.resolve_code_path(package) {
            Err(e) => e,
            Ok(code_dir) => match lance::fetch_symbol_source(&code_dir, symbol) {
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
                    if src.signature.is_empty() {
                        format!(
                            "**{}** ({}) @ {}\n\n```\n{}\n```",
                            src.symbol, src.kind, loc, src.content
                        )
                    } else {
                        format!(
                            "**{}** ({}) @ {}\n{}\n\n```\n{}\n```",
                            src.symbol, src.kind, loc, src.signature, src.content
                        )
                    }
                }
            },
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
    /// Both the reranked and BM25 paths produce the same compact text format;
    /// reranked results include a relevance score prefix `[0.92]`.
    fn apply_rerank_to_codegraph_json(
        &self,
        query: &str,
        raw_json: &str,
        output_n: usize,
    ) -> String {
        let mut guard = self.reranker.borrow_mut();
        let Some(ranker) = guard.as_mut() else {
            return raw_json.to_string();
        };

        let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(raw_json) else {
            return raw_json.to_string();
        };

        let mut node_map: HashMap<String, VecDeque<serde_json::Value>> = HashMap::new();
        for value in arr.iter().cloned() {
            let node = value.get("node").unwrap_or(&value);
            let qual = node
                .get("qualifiedName")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let name = node
                .get("name")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let key = if !qual.is_empty() { qual } else { name };
            if !key.is_empty() {
                node_map.entry(key).or_default().push_back(value);
            }
        }

        let candidates = reranker::codegraph_json_to_candidates(raw_json);
        if candidates.is_empty() {
            return raw_json.to_string();
        }

        match ranker.rerank(query, candidates) {
            Ok(mut scored) => {
                scored.truncate(output_n);
                if scored.is_empty() {
                    return "[]".to_string();
                }

                let mut reranked = Vec::new();
                for result in scored {
                    if let Some(values) = node_map.get_mut(&result.source) {
                        if let Some(value) = values.pop_front() {
                            reranked.push(value);
                        }
                    }
                }

                if reranked.is_empty() {
                    return raw_json.to_string();
                }

                serde_json::to_string(&reranked).unwrap_or_else(|_| raw_json.to_string())
            }
            Err(e) => {
                eprintln!("sempkg: reranker error ({e}), returning BM25 results");
                raw_json.to_string()
            }
        }
    }

    fn apply_rerank_to_codegraph(&self, query: &str, raw_json: &str, output_n: usize) -> String {
        let mut guard = self.reranker.borrow_mut();
        let Some(ranker) = guard.as_mut() else {
            return fmt_codegraph_json(raw_json);
        };

        // Build a lookup map: qualified-name (or name) → node Value so the
        // structured format can be reconstructed after reranking.
        let node_map: HashMap<String, serde_json::Value> =
            serde_json::from_str::<Vec<serde_json::Value>>(raw_json)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|v| {
                    let node = v.get("node").cloned().unwrap_or(v);
                    let qual = node
                        .get("qualifiedName")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = node
                        .get("name")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    let key = if !qual.is_empty() { qual } else { name };
                    if key.is_empty() {
                        None
                    } else {
                        Some((key, node))
                    }
                })
                .collect();

        let candidates = reranker::codegraph_json_to_candidates(raw_json);
        if candidates.is_empty() {
            return fmt_codegraph_json(raw_json);
        }

        match ranker.rerank(query, candidates) {
            Ok(mut scored) => {
                scored.truncate(output_n);
                if scored.is_empty() {
                    return format!("No results for '{query}'.");
                }
                scored
                    .iter()
                    .map(|r| {
                        if let Some(node) = node_map.get(&r.source) {
                            fmt_codegraph_hit(node, Some(r.score))
                        } else {
                            format!("[{:.2}] {}", r.score, r.source)
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            Err(e) => {
                eprintln!("sempkg: reranker error ({e}), returning BM25 results");
                fmt_codegraph_json(raw_json)
            }
        }
    }

    /// Rerank LanceDB search results.
    /// Both paths produce the same format; reranked results include a `[0.92]` score prefix.
    fn apply_rerank_to_lance(
        &self,
        query: &str,
        results: Vec<lance::SearchResult>,
        output_n: usize,
    ) -> String {
        let mut guard = self.reranker.borrow_mut();
        let Some(ranker) = guard.as_mut() else {
            return fmt_lance_results(&results, None);
        };

        let candidates = reranker::lance_results_to_candidates(&results);
        if candidates.is_empty() {
            return fmt_lance_results(&results, None);
        }

        match ranker.rerank(query, candidates) {
            Ok(mut scored) => {
                scored.truncate(output_n);
                // Build score map: loc_key → score
                let score_map: HashMap<String, f32> =
                    scored.iter().map(|r| (r.source.clone(), r.score)).collect();
                // Re-order the original results to match the reranked order,
                // keeping only those that appear in the scored set.
                let mut ordered: Vec<lance::SearchResult> = results
                    .iter()
                    .filter(|r| {
                        let key = if r.start_line > 0 {
                            format!("{}:{}-{}", r.path, r.start_line, r.end_line)
                        } else {
                            r.path.clone()
                        };
                        score_map.contains_key(&key)
                    })
                    .cloned()
                    .collect();
                ordered.sort_by(|a, b| {
                    let ka = if a.start_line > 0 {
                        format!("{}:{}-{}", a.path, a.start_line, a.end_line)
                    } else {
                        a.path.clone()
                    };
                    let kb = if b.start_line > 0 {
                        format!("{}:{}-{}", b.path, b.start_line, b.end_line)
                    } else {
                        b.path.clone()
                    };
                    let sa = score_map.get(&ka).copied().unwrap_or(0.0);
                    let sb = score_map.get(&kb).copied().unwrap_or(0.0);
                    sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
                });
                fmt_lance_results(&ordered, Some(&score_map))
            }
            Err(e) => {
                eprintln!("sempkg: reranker error ({e}), returning BM25 results");
                fmt_lance_results(&results, None)
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
        // Accept both "name" and "name@version" — query hits carry the version.
        if let Some(bundle) = resolve_bundle_spec(name, self.workspace().map(|p| p.as_path())) {
            if !bundle.has_code() {
                return Err(format!(
                    "Bundle '{}@{}' does not have a source-code index. \
                     Rebuild with 'sembundle build --include-source'.",
                    bundle.name, bundle.version
                ));
            }
            return Ok(bundle.bundle_dir.join("code"));
        }
        Err(format!(
            "'{name}' not found or has no source-code index. \
             Use 'sempkg list' to see available bundles."
        ))
    }

    /// Format a codegraph JSON array as a compact symbol list, inlining source
    /// bodies from the code index when available (within a byte budget).
    fn fmt_codegraph_with_source(&self, package: &str, codegraph_json: &str) -> String {
        let arr = match serde_json::from_str::<Vec<serde_json::Value>>(codegraph_json) {
            Ok(a) => a,
            Err(_) => return codegraph_json.to_string(),
        };
        if arr.is_empty() {
            return "No results.".to_string();
        }

        let code_dir = self.resolve_code_path(package).ok();
        const BYTE_BUDGET: usize = 12_000;
        let mut total_bytes = 0usize;
        let mut sections: Vec<String> = Vec::new();

        for v in &arr {
            let node = v.get("node").unwrap_or(v);
            let header = fmt_codegraph_hit(node, None);

            // Attempt to inline the source body when a code index is present.
            if let Some(ref dir) = code_dir {
                let get_str = |k: &str| node.get(k).and_then(|x| x.as_str()).unwrap_or("");
                let qualified = get_str("qualifiedName");
                let name = get_str("name");
                let sym = if !qualified.is_empty() {
                    qualified
                } else {
                    name
                };
                if !sym.is_empty() {
                    if let Ok(lance::SymbolLookup::Unique(src)) =
                        lance::fetch_symbol_source(dir, sym)
                    {
                        if total_bytes + src.content.len() <= BYTE_BUDGET {
                            total_bytes += src.content.len();
                            sections.push(format!("{header}\n\n```\n{}\n```", src.content));
                            continue;
                        }
                    }
                }
            }
            sections.push(header);
        }

        sections.join("\n\n---\n\n")
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
            "get_context" => {
                self.tool_get_context(str_arg("package"), str_arg("task"), int_arg("limit", 20))
            }
            "get_callers" => {
                self.tool_get_callers(str_arg("package"), str_arg("symbol"), int_arg("limit", 20))
            }
            "get_callees" => {
                self.tool_get_callees(str_arg("package"), str_arg("symbol"), int_arg("limit", 20))
            }
            "get_impact" => {
                self.tool_get_impact(str_arg("package"), str_arg("symbol"), int_arg("depth", 3))
            }
            "list_files" => {
                self.tool_list_files(str_arg("package"), opt_str("filter"), int_arg("limit", 200))
            }
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
                let line = args.get("line").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                self.tool_read_code(str_arg("package"), str_arg("file"), line)
            }
            "query" => self.tool_query(str_arg("query"), int_arg("limit", 10)),
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

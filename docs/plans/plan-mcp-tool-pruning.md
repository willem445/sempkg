# Implementation Plan: Prune & Refocus the MCP Tool Surface

> **Status:** Proposed
> **Date:** 2026-06-24
> **Scope:** `src/sempkg/src/mcp.rs`, `src/sempkg/src/lance.rs`,
> `.github/agents/sempkg.agent.md`, and (optionally) the sembundle build path.
> **Goal:** Collapse the current 13-tool MCP surface into a focused, layered set
> built around `query` as the primary discovery entry point, with clear
> read and graph-traversal tiers underneath.

---

## 1. Motivation

The `query` tool is now the unified cross-package discovery entry point: it fans
out to every code, docs, and CodeGraph backend, deduplicates, fuses with RRF, and
reranks. That makes several of the original single-backend search tools redundant
— they duplicate slices of what `query` already does, and they force the agent to
choose a retrieval backend instead of asking a question.

We want a smaller, layered surface that maps cleanly to how an agent actually
works:

| Tier | Purpose | Tools |
|------|---------|-------|
| **Discover** | Find *where* relevant code/docs live | `query`, `list_packages`, `list_files` |
| **Read** | Pull exact content at a known location | `read_code` (by line range), `read_docs`, `read_symbol` |
| **Traverse** | Walk the call graph from a known symbol | `get_callers`, `get_callees`, `get_impact` |

Result: **9 tools** instead of 13.

---

## 2. Tool-by-tool decisions

| Current tool | Decision | Rationale |
|--------------|----------|-----------|
| `query` | **Keep** (enhance) | Primary discovery entry point. Add relevance highlighting (§5). |
| `list_packages` | **Keep** | Discovery: what is indexed. |
| `list_files` | **Keep** | Discovery: what files exist in a package. |
| `read_symbol` | **Keep** | Read a full symbol body by name. |
| `read_code` | **Keep** (redesign) | Change from "enclosing-symbol-at-line" to **line-range reads** (§4). |
| `get_callers` | **Keep** | Graph traversal. |
| `get_callees` | **Keep** | Graph traversal. |
| `get_impact` | **Keep** | Graph traversal. |
| `search_symbols` | **Remove** | Superseded by `query` (CodeGraph is one of its fan-out backends). |
| `search_code` | **Remove** | Superseded by `query` (code index is a fan-out backend). |
| `get_context` | **Remove** | NL CodeGraph context is fully subsumed by `query`'s reranked hybrid retrieval. |
| `docs_metadata` | **Remove** | Low value; `+lance` status is already shown by `list_packages`. |
| `search_docs` | **Replace** → `read_docs` | Doc *search* is part of `query`; what's missing is granular doc *reading* (§6). |

### Net surface

```
query            list_packages    list_files        ← discover
read_code        read_docs        read_symbol       ← read
get_callers      get_callees      get_impact        ← traverse
```

---

## 3. Central technical decision: where does raw content come from?

The user's intent for `read_code` is "read by line numbers, similar to how it
reads sections of documents when working normally" — i.e. arbitrary line-range
addressing within a file.

**Current storage reality:** the bundle's `code` LanceDB table stores **symbol
chunks**, not whole source files. Each row carries `path`, `content`,
`start_line`, `end_line`, `start_byte`, `end_byte` for a single top-level symbol.
There is no row for code *between* symbols (imports, module-level statements,
comments, blank regions). The docs table is the same shape but with
`start_line = 0` (no line metadata).

This forces a choice for true arbitrary-range reads:

- **Option A — Chunk reconstruction (no format change).**
  `read_code(file, start, end)` selects the `code`-table chunk(s) whose
  `[start_line, end_line]` overlaps the requested range and slices them to the
  requested lines. Cheap, ships immediately, but **cannot** return lines that
  fall outside any indexed symbol (gaps return "no content for these lines").

- **Option B — Store raw source files (sembundle format change).** Add an
  optional `source/` file tree (or a `files` LanceDB table keyed by path holding
  full file text) when building with `--include-source`. `read_code` then slices
  any requested line range from the full file, exactly like a normal editor read.
  Larger bundles; requires sembundle `pack`/`manifest`/`validate` changes and a
  `spec_version` bump.

**Recommendation:** ship **Option A first** (Phase 1) so the refocused surface
lands without a format change, then add **Option B** (Phase 3) as the
`--include-full-source` capability for true editor-grade reads. `read_code`'s
external contract is identical for both; only the resolver behind it changes, so
Phase 3 is non-breaking.

---

## 4. `read_code` redesign — line-range reads

### 4.1 New schema

```jsonc
{
  "name": "read_code",
  "inputSchema": {
    "package":    "string  (required)",
    "file":       "string  (required) — source path as returned by query/list_files",
    "start_line": "integer (optional) — 1-based; omit to start at the symbol/file start",
    "end_line":   "integer (optional) — 1-based inclusive; omit to read to symbol/file end",
    "line":       "integer (optional, deprecated alias) — enclosing-symbol read at this line"
  },
  "required": ["package", "file"]
}
```

Behaviour:
- `start_line` + `end_line` given → return exactly those lines.
- only `line` given (back-compat) → return the enclosing symbol (current behaviour
  via `fetch_symbol_at_location`).
- nothing given → return the whole file (Option B) or all chunks for the file
  joined in line order (Option A).

### 4.2 Implementation (Phase 1, Option A)

Add `lance::read_code_lines(code_dir, file, start_line, end_line) -> Result<Option<CodeSlice>>`:

1. Open the `code` table; filter by normalised `path` (reuse the
   `path = ? OR path LIKE '%/?' OR path LIKE '%\\?'` pattern already in
   `fetch_from_code_table`).
2. Collect all chunks for that file, sort by `start_line`.
3. Keep chunks overlapping `[start_line, end_line]`; slice each chunk's `content`
   to the requested lines using its `start_line` offset; concatenate in order.
4. Return `CodeSlice { path, start_line, end_line, content, covered: bool }` where
   `covered = false` signals that some requested lines were not present in any
   indexed symbol (so `read_code` can tell the agent to widen the range or that
   Phase 3 full-source is needed).

`tool_read_code` in `mcp.rs` dispatches on which args were supplied.

### 4.3 Dispatch change

In `dispatch_tool`:

```rust
"read_code" => {
    let start = args.get("start_line").and_then(|v| v.as_u64()).map(|n| n as u32);
    let end   = args.get("end_line").and_then(|v| v.as_u64()).map(|n| n as u32);
    let line  = args.get("line").and_then(|v| v.as_u64()).map(|n| n as u32);
    self.tool_read_code(str_arg("package"), str_arg("file"), start, end, line)
}
```

---

## 5. `query` enhancement — relevance highlighting

Goal (from the request): when a code hit is a function/large symbol, surface the
**most relevant lines** rather than dumping the whole body; when only the symbol
*name* is what's relevant, return just the name + location and let the agent drill
in with `read_code`/`read_symbol`.

### 5.1 Highlight strategy

For each code/codegraph hit selected for output in `tool_query`:

1. If the body is short (≤ N lines, e.g. 20), keep the existing snippet block.
2. If the body is long, run a **sub-symbol relevance pass**:
   - Split the body into small windows (e.g. 4–6 line sliding windows or
     statement groups).
   - Score each window against the query. Reuse the loaded reranker when
     available; otherwise fall back to BM25/term-overlap scoring (no model
     dependency).
   - Emit only the top 1–2 windows as the highlighted snippet, annotated with
     their line numbers, plus a one-line "full body: `read_code(pkg, file,
     {start}, {end})`" pointer.
3. If no window clears a minimum relevance bar, emit **name + kind + location
   only** (no body) — the agent traverses deeper on demand.

### 5.2 Touch points

- `format_unified_hit` (mcp.rs ~L450): accept an optional `highlight: Option<Highlight>`
  and render the highlighted lines + the `read_code` pointer instead of the raw
  `snippet`.
- New helper `highlight_relevant_lines(query, hit, reranker) -> Option<Highlight>`
  invoked from `tool_query` for the final, post-rerank result set only (bounded
  cost: `limit` hits, not the whole pool).

This is the highest-value but most involved piece; it can ship in Phase 2 after
the prune lands, since the surface is correct without it.

---

## 6. `read_docs` — granular documentation reads

Replaces `search_docs` (search is now `query`'s job). `read_docs` reads a known
document, mirroring `read_code`.

### 6.1 Schema

```jsonc
{
  "name": "read_docs",
  "inputSchema": {
    "package": "string (required)",
    "path":    "string (required) — doc path as returned by query",
    "section": "string  (optional) — heading/anchor substring to scope the read",
    "limit":   "integer (optional) — max chars/chunks to return (default e.g. 4000 chars)"
  },
  "required": ["package", "path"]
}
```

### 6.2 Behaviour & implementation

Docs chunks carry `start_byte`/`end_byte` but `start_line = 0`, so addressing is
by document + (optional) section rather than line numbers:

- Add `lance::read_doc(lance_dir, path, section, limit) -> Result<Option<DocRead>>`:
  1. Filter the `docs` table by normalised `path`.
  2. Order chunks by `start_byte` and concatenate to reconstruct the document
     (deduping the `path#fragment` suffix already stripped in `search_table`).
  3. If `section` is given, scope to the matching heading/anchor region.
  4. Truncate to `limit`, returning a `DocRead { path, section, content,
     truncated }`.
- `tool_read_docs` formats the result as markdown.

---

## 7. Files to change

| File | Change |
|------|--------|
| `src/sempkg/src/mcp.rs` | Remove `search_symbols`, `search_code`, `get_context`, `docs_metadata` from `all_tools()` and `dispatch_tool`; delete their `tool_*` methods. Replace `search_docs`→`read_docs`. Redesign `read_code` schema + dispatch. Add highlighting to `tool_query`/`format_unified_hit`. Update the "use search_* instead" hint strings in the no-result branches. |
| `src/sempkg/src/lance.rs` | Add `read_code_lines` (Option A) and `read_doc`; keep `fetch_symbol_source`/`fetch_symbol_at_location` for `read_symbol` and the `line` back-compat path. `search_code`/`search` remain (still used internally by `tool_query`). |
| `.github/agents/sempkg.agent.md` | Rewrite the tool-selection table and workflow to the 3-tier model. Remove references to `search_symbols`, `search_code`, `get_context`, `search_docs`, `docs_metadata`. |
| `docs/design/query-tool-design.md` | Note the surface reduction and the highlighting addition. |
| `docs/plans/plan-sembundle-source-code-index.md` | Cross-reference; Phase 3 full-source storage extends it. |
| `src/sembundle/*` *(Phase 3 only)* | `--include-full-source` build flag, `source/` packaging, `manifest`/`validate` updates, `spec_version` bump. |

> Note: `tool_query` still calls `lance::search`, `lance::search_code`, and
> `codegraph::query` internally — removing the *MCP tools* `search_docs` /
> `search_code` / `search_symbols` does **not** remove these library functions.

---

## 8. Phasing

1. **Phase 1 — Prune + read redesign (no format change).**
   - Drop the four redundant tools; replace `search_docs` with `read_docs`.
   - Redesign `read_code` for line ranges (Option A chunk reconstruction).
   - Update the agent definition and docs.
   - Outcome: the 9-tool surface is live and self-consistent.
2. **Phase 2 — Query highlighting.**
   - Add sub-symbol relevance highlighting + name-only fallback to `tool_query`.
3. **Phase 3 — Full-source storage (sembundle format change, optional).**
   - `--include-full-source`; `read_code` gains true arbitrary line reads for
     any line (including gaps between symbols). Non-breaking for `read_code`'s
     contract.

---

## 9. Testing

- `tests/test_mcp_functional.py`: update the expected `tools/list` set to the 9
  tools; assert the removed tools are gone. Add cases for `read_code` with
  `start_line`/`end_line`, the `line` back-compat path, and `read_docs`.
- Rust unit tests in `lance.rs` for `read_code_lines` (overlap slicing, gap
  reporting) and `read_doc` (chunk reassembly, section scoping).
- `cargo fmt`, `cargo clippy --all-targets --all-features -- -D warnings`,
  `cargo test`, and `pytest` before finalizing.

---

## 10. Backwards-compatibility notes

- Removing tools is a visible MCP surface change. Any external automation calling
  `search_symbols`/`search_code`/`get_context`/`search_docs`/`docs_metadata`
  must migrate to `query` (search) or `read_docs` (doc reads). Call this out in
  the changelog.
- `read_code`'s `line` parameter is retained as a deprecated alias so existing
  callers that pass a single line keep working.

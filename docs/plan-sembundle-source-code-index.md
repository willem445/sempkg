# Implementation Plan: Embedded Source-Code Index (`--include-source`)

> **Status:** Proposed
> **Audience:** Implementation model / engineer
> **Goal:** Let a `.sembundle` optionally carry the *actual source code*, chunked by
> top-level symbol using CodeGraph's graph data and stored in a LanceDB table —
> so agents can do a fast semantic/keyword search **and** pull the real code body
> for deeper reads, all from the same bundle, with no repo clone.

---

## 1. Objective

Today a bundle stores a semantic graph (`graph/`), embeddings (`embeddings/`), and
an optional documentation index (`lance/` → `docs.lance`). The graph tells an agent
*where* a symbol lives (path + line range) but the bundle does **not** contain the
source itself; the body must be fetched from the upstream repo at `commit_hash`.

This feature adds a **second, optional LanceDB table** holding the source code,
chunked by **top-level symbol** (function / class / method) rather than by fixed
character windows, plus the line/byte spans needed to slice the original file.

End-to-end target:

```bash
# Build a bundle that also embeds the source code, chunked by symbol.
sembundle build \
  --name pandas --version 2.2.2 \
  --source-repo https://github.com/pandas-dev/pandas \
  --commit-hash <sha> --codegraph-version 0.9.7 \
  --source-dir ./pandas \
  --include-source
```

Then an agent connected to the MCP server can:

1. `search_symbols` / `search_code` to find candidates (fast, BM25 + optional rerank).
2. `get_code` / `read_symbol` to pull the **full source body** of a chosen symbol.
3. `get_callers` / `get_callees` and receive the **source of each caller/callee**
   inline (the "followup deep read" in a single round-trip).

`--include-source` is **opt-in**. Default behaviour and existing bundles are unchanged.

---

## 2. Current Architecture (context for the implementer)

Two independent Rust crates:

- `src/sembundle/` — packs / builds / signs / publishes bundles.
  - [build.rs](../src/sembundle/src/build.rs): `build(BuildOptions)`
    - `run_codegraph(...)` shells out to `codegraph init --index <dir>` and copies
      `.codegraph/` into `graph/`.
    - `run_lance(...)` builds the docs LanceDB table (`path`, `content` columns only).
    - `pack(...)` writes the final archive.
  - [pack.rs](../src/sembundle/src/pack.rs): `pack(PackOptions)`; today it copies
    `graph/`, `embeddings/`, `config.json`, generates `metadata.json` /
    `manifest.json`, and — when `lance_dir: Some(_)` — calls
    `collect_lance_entries(...)` and appends `"lance"` to `manifest.extensions`.
  - [manifest.rs](../src/sembundle/src/manifest.rs): `Manifest`, `Metadata`,
    `LanceMetadata` structs; `spec_version` currently `1.2.0`.
  - [validate.rs](../src/sembundle/src/validate.rs): `validate_lance_dir(...)`.
  - [main.rs](../src/sembundle/src/main.rs): clap CLI for `Build` and `Pack`.

- `src/sempkg/` — manager + MCP server + scoped queries.
  - [lance.rs](../src/sempkg/src/lance.rs): the richer LanceDB code path.
    - `cli_update(...)` walks a dir, `chunk_text(...)` chunks on `\n\n` paragraphs,
      writes table `docs` with columns `path, content, start_line, end_line,
      start_byte, end_byte`, builds an FTS index on `content`, writes `metadata.json`.
    - `search(lance_dir, query, limit)` runs BM25 `full_text_search` and returns
      `SearchResult { path, snippet, start_line, end_line, start_byte, end_byte }`.
    - `format_results(...)`, `has_lance(...)`, `LanceMetadata`.
  - [codegraph.rs](../src/sempkg/src/codegraph.rs): thin wrappers around the
    external `codegraph` CLI — `query`, `callers`, `callees`, `context`, `impact`,
    `files`, all returning JSON via `--json`.
  - [mcp.rs](../src/sempkg/src/mcp.rs): JSON-RPC server.
    - `all_tools()` lists tool schemas; `dispatch_tool(name, args)` routes calls.
    - `tool_get_callers/tool_get_callees` call `codegraph::callers/callees`.
    - `tool_search_docs` calls `lance::search` + optional rerank.
    - `resolve_lance_path` / `resolve_codegraph_path` scope every call to one bundle.

**Key observation:** the source-code index is *structurally identical* to the docs
index — same LanceDB + FTS machinery, same `SearchResult` shape. The only genuinely
new work is (a) **symbol-aware chunking** driven by graph data, and (b) wiring a
**second table** through pack/manifest/MCP.

---

## 3. Design Decisions

### 3.1 Storage layout (new optional extension `code`)

```
bundle/
├── manifest.json          # extensions now may include "code"
├── metadata.json
├── config.json
├── graph/
├── embeddings/
├── lance/                 # docs (optional, unchanged)
│   ├── metadata.json
│   └── docs.lance/
└── code/                  # NEW (optional)
    ├── metadata.json      # code-index metadata
    └── code.lance/        # LanceDB table, FTS index on `content`
```

- New extension name: **`"code"`** (parallels `"lance"`).
- `manifest.extensions` gains `"code"` when present; bump `spec_version` to `1.3.0`.
- Keeping `code/` as a *separate* table (not a column added to `docs.lance`) means
  zero impact on existing readers and lets docs/code be searched independently.

### 3.2 Table schema (`code.lance`)

Superset of the docs schema so `lance::search` can be reused with minimal change:

| column        | type   | notes |
|---------------|--------|-------|
| `path`        | Utf8   | repo-relative source file path |
| `symbol`      | Utf8   | fully-qualified symbol name (e.g. `pandas.core.frame.DataFrame.merge`) |
| `kind`        | Utf8   | `function` / `class` / `method` / `module` |
| `content`     | Utf8   | the **source text** of the symbol (the chunk) — FTS-indexed |
| `signature`   | Utf8   | first line / declaration (cheap preview), may be empty |
| `start_line`  | UInt32 | 1-based |
| `end_line`    | UInt32 | 1-based |
| `start_byte`  | UInt32 | |
| `end_byte`    | UInt32 | |

`SearchResult` in [lance.rs](../src/sempkg/src/lance.rs) gains optional
`symbol`/`kind`/`signature` fields (defaulted empty for the docs table).

### 3.3 Symbol-aware chunking strategy

Instead of `chunk_text` (paragraph windows), build code chunks from graph data:

1. After `run_codegraph` has produced `.codegraph/`, enumerate **top-level**
   symbols with their file + line spans. Two viable sources:
   - **Preferred:** read CodeGraph's SQLite DB directly
     (`.codegraph/codegraph.db`) for `(symbol, kind, path, start_line, end_line)`.
   - **Fallback / portable:** shell out to the `codegraph` CLI (e.g.
     `codegraph files --json` to list files, then `codegraph query <name> --json`),
     accepting whatever span metadata it emits.
   - Decide at implementation time which the installed `codegraph` version exposes;
     gate behind a small `symbols.rs` adapter so the rest of the pipeline is stable.
2. "Top-level" = symbols whose parent is a file/module (functions, classes). A class
   is emitted as **one chunk** (whole body); optionally also emit each method as its
   own chunk for finer granularity (config flag `--source-granularity symbol|method`,
   default `symbol`).
3. For each symbol, slice the source file `[start_byte..end_byte]` (or by line if
   only line spans are available) to get `content`. Capture `signature` = first
   non-blank line of the slice.
4. **Oversized symbols:** if a chunk exceeds a max (e.g. 8 KB), split into ordered
   sub-chunks sharing the same `symbol`/`path` with a `part` suffix, mirroring the
   existing oversized-paragraph handling in `chunk_text`.
5. **Files with no recognized symbols** (configs, scripts): optionally fall back to
   `chunk_text`-style windowing so nothing is silently dropped (flag, default off to
   keep bundles lean).

> Reuse `byte_to_line` and the FTS-build/`create_table` block in
> [lance.rs](../src/sempkg/src/lance.rs); only the *chunk producer* differs.

### 3.4 Bundle size

Embedding source roughly doubles raw text vs. graph-only. Mitigations:
- Opt-in flag only.
- Gzip already applied by the tar writer in `pack`.
- `code/metadata.json` records `symbol_count`, `chunk_count`, `byte_size` so
  `sempkg info` can surface the cost.

---

## 4. Implementation Steps

### Step 1 — `sembundle`: chunking + table builder
- **New** `src/sembundle/src/source_index.rs` (or extend `build.rs`):
  - `struct SymbolChunk { path, symbol, kind, signature, content, start_line, end_line, start_byte, end_byte }`.
  - `fn collect_symbol_chunks(source_dirs, granularity) -> Vec<SymbolChunk>`:
    enumerate symbols (Step 3.3) and slice source files.
  - `fn run_source_lance(chunks, out_dir) -> Result<()>`: write `code.lance` with the
    schema in §3.2 and an FTS index on `content`; write `code/metadata.json`.
  - **New** `src/sembundle/src/symbols.rs`: the CodeGraph adapter (SQLite read or CLI
    shell-out) returning `(symbol, kind, path, start_line, end_line)` tuples.

### Step 2 — `sembundle`: pack + manifest wiring
- [pack.rs](../src/sembundle/src/pack.rs):
  - Add `pub code_dir: Option<PathBuf>` to `PackOptions`.
  - Mirror the lance block: `collect_code_entries(code_dir, &created_at, &mut entries)?;`
    and `extensions.push("code")`. Factor the existing `collect_lance_entries` into a
    generic `collect_extension_dir(dir, top_level_key, ...)` to avoid duplication.
- [manifest.rs](../src/sembundle/src/manifest.rs): bump `spec_version` to `"1.3.0"`;
  reuse `LanceMetadata` or add a small `CodeMetadata` if extra fields (symbol_count) warrant.
- [validate.rs](../src/sembundle/src/validate.rs): add `validate_code_dir` (requires
  `metadata.json` + `code.lance/`).

### Step 3 — `sembundle`: CLI flags
- [main.rs](../src/sembundle/src/main.rs), `Build` subcommand:
  - `--include-source` (`bool`).
  - `--source-granularity` (`symbol` | `method`, default `symbol`).
  - Optional `--source-glob` to restrict which files are sliced (default: all indexed).
- [build.rs](../src/sembundle/src/build.rs), `BuildOptions` + `build()`:
  - When `include_source`, after `run_codegraph`, call
    `collect_symbol_chunks` → `run_source_lance` into `work/code-out`, then pass
    `code_dir: Some(...)` to `pack`.
- (Optional) Also expose `--code-dir` on the lower-level `Pack` subcommand for users
  who pre-build the table, symmetric with the existing `--lance-dir`.

### Step 4 — `sempkg`: read path
- [lance.rs](../src/sempkg/src/lance.rs):
  - Generalize `search` to accept a `table_name` (`"docs"` today, `"code"` for source),
    or add `search_code(...)` thin wrapper. Add the new optional columns to
    `SearchResult` and populate when present.
  - Add `has_code(bundle_dir)` and `code_dir_path(bundle_dir)` helpers.
- [store.rs / main.rs](../src/sempkg/src/store.rs): add `resolve_code_path(name, workspace)`
  parallel to `resolve_lance_path`; surface `+code` in `sempkg list` / `info`
  (mirrors the existing `+lance` flag).

### Step 5 — `sempkg`: MCP tools
- [mcp.rs](../src/sempkg/src/mcp.rs), in `all_tools()` add:
  - **`search_code`** — BM25 search over the source table. Params `package, query, kind?, limit?`.
    Returns `path:lines`, `symbol`, and a `content` excerpt. Reuse the rerank path
    (`apply_rerank_to_lance`).
  - **`read_symbol`** (a.k.a. `get_code`) — exact fetch of a symbol's full body by
    `package` + `symbol` (+ optional `path` to disambiguate). Does a filtered LanceDB
    lookup on the `symbol` column and returns the complete `content`, not a snippet.
- `dispatch_tool`: route `"search_code"` / `"read_symbol"`.

### Step 6 — `get_callers` / `get_callees` deep reads
- [mcp.rs](../src/sempkg/src/mcp.rs) `tool_get_callers` / `tool_get_callees`:
  - Keep current behaviour (codegraph JSON of caller/callee symbols).
  - **New:** when the bundle `has_code`, add a `with_source` arg (default `true` if the
    code table exists). For each returned caller/callee symbol, look it up in
    `code.lance` and append its source body (or signature + first N lines when large).
  - Output format: a Markdown section per symbol — `**caller_symbol** (path:lines)`
    followed by a fenced code block of the body. Respect `limit` and a per-call
    byte budget so responses stay bounded.
  - New helper `fn fetch_symbol_source(code_dir, symbol) -> Option<SymbolSource>` in
    [lance.rs](../src/sempkg/src/lance.rs), reused by `read_symbol` and the
    caller/callee augmentation.

### Step 7 — Spec + docs
- [sembundle-spec.md](sembundle-spec.md): add the `code/` extension (§ parallel to
  the lance §), document the `code.lance` schema, list `"code"` as a valid
  `extensions` value, and bump `spec_version` to `1.3.0`.
- README: short note on `--include-source` and the new MCP tools.

### Step 8 — Tests
- `sembundle`: unit test `collect_symbol_chunks` against a tiny fixture repo with
  known symbols/line spans; round-trip `pack` → extract → assert `code/code.lance` +
  manifest `extensions` contains `"code"` + checksums verify.
- `sempkg`: `search_code` returns the seeded symbol; `read_symbol` returns the full
  body; `get_callers --with-source` includes caller bodies; bundles **without** a
  code table behave exactly as before (no regressions in `search_docs`).
- Back-compat: a `1.2.0` bundle (no `code/`) still validates and loads.

---

## 5. Backwards Compatibility

- Purely additive: no required entry changes. Readers ignore unknown extensions.
- `spec_version` bump to `1.3.0`; validators must accept bundles **without** `code/`.
- All new MCP tools are no-ops (clear "no source index in this bundle" message) when
  the `code` extension is absent.

---

## 6. Open Questions

1. **Symbol source of truth** — does the installed `codegraph` expose end-line/byte
   spans for symbols via CLI, or must we read `.codegraph/codegraph.db` directly?
   This determines the `symbols.rs` adapter and is the main implementation risk.
2. **Granularity default** — whole-symbol chunks (simple, slightly coarse) vs.
   per-method chunks (finer search, larger table). Proposed default: `symbol`.
3. **Non-symbol files** — drop them, or window-chunk as a fallback? Proposed: drop by
   default, opt-in fallback flag.
4. **De-dup with embeddings** — should code chunks also get vector embeddings for
   semantic (not just BM25) search, or is FTS + reranker enough for v1? Proposed:
   FTS + existing reranker for v1; vectors are a follow-up.

---

## 7. Suggested Sequencing

1. Steps 1–3 (sembundle: produce a `code/` extension) — independently shippable;
   verify via `sembundle build --include-source` + manual extract.
2. Steps 4–5 (sempkg read + `search_code` / `read_symbol`).
3. Step 6 (caller/callee deep reads).
4. Steps 7–8 (spec, docs, tests) alongside each stage.

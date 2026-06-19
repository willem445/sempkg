# ADR-001: Replace QMD with LanceDB for Documentation Indexing

**Date:** 2026-06-15
**Status:** Accepted
**Deciders:** sempkg maintainers

---

## Context

SemBundle 1.1.0 introduced an optional `qmd/` extension for bundling documentation search.
It worked by invoking the external **QMD** CLI (a Node.js tool) to:

1. Walk documentation directories (`*.md`, `*.txt`, `*.rst`)
2. Chunk text and embed it using a GGUF model
3. Store the results in an SQLite database (`qmd/index/index.sqlite`)

At query time, `sempkg` opened the SQLite file via **rusqlite** (with a bundled SQLite3 build)
and ran FTS5 queries against it. The Python `sempkg` server did the same via `sqlite3`.

### Problems with QMD

| Problem | Detail |
|---------|--------|
| **External dependency** | QMD must be installed separately via `npm install -g @tobilu/qmd`. Breaks CI and containerised deployments that have no Node.js runtime. |
| **Large model download** | QMD downloads a GGUF embedding model (~300 MB) on first run to produce vector embeddings. This made cold-build times unacceptable. |
| **Windows reliability** | QMD's npm package has known issues on Windows paths with spaces and on PowerShell. |
| **No portable embeddings** | The embedding vectors were stored inside the SQLite file in sqlite-vec format; reading them from Rust required matching the sqlite-vec native extension, which added a second native build dependency. |
| **FTS only through SQLite FTS5** | SQLite FTS5 is good but cannot be tuned independently of the SQLite build. |
| **Spec drift** | The `qmd/` extension accumulated implementation-defined sub-paths (`embeddings/`, `model.gguf`, `config.json`) not cleanly described by the spec. |

---

## Decision

Replace the `qmd/` bundle extension and all associated tooling with **LanceDB**:

- The `qmd/` directory in bundles is replaced by `lance/`
- The external `qmd` CLI is removed; indexing is done **in-process in pure Rust**
- The `rusqlite` dependency is removed from `sempkg`
- `lancedb`, `arrow-array`, `arrow-schema`, and `futures` are added to `sempkg` and `SemBundle`
- `lancedb` is added to `sempkg` Python dependencies

The bundle spec is bumped from **1.1.0 → 1.2.0**.

---

## Considered Alternatives

### A: Keep QMD, make it optional

Make the QMD indexing step a no-op when QMD is not installed. This solves the CI problem
but leaves the Windows reliability issue and the large model download in place. Rejected
because the root cause (external Node.js tool) is unchanged.

### B: Use tantivy directly (no LanceDB)

tantivy is the Rust BM25 search engine used internally by LanceDB. Using it directly would
be lighter weight but requires implementing file-based persistence and a query API from
scratch. LanceDB provides both, plus the Arrow columnar format gives portable, inspectable
storage with no proprietary lock-in. Rejected to avoid reinventing what LanceDB already
provides.

### C: Use Meilisearch embedded

Meilisearch has an embedded Rust mode but it is not designed for portable archive
distribution (it maintains mutable index state). Rejected because bundle immutability
is a first-class requirement of the SemBundle spec.

### D: Use LanceDB (chosen)

LanceDB stores data as **Arrow Lance files** — a columnar format that is:
- Self-contained (a directory, trivially archived)
- Immutable once written (read-only consumers never mutate it)
- Portable across OS/architecture without recompilation
- Queryable via a pure-Rust async API

It ships **tantivy BM25 full-text search** as a first-class index type, matching the
quality of QMD's FTS5 search without any external tooling.

---

## Consequences

### Positive

- **No external tools required** for indexing or querying. `SemBundle build --docs-dir` runs entirely in-process.
- **No model download.** BM25 is statistical; no embedding model is needed for keyword search.
- **Windows-native.** LanceDB's Rust crate compiles cleanly on all platforms.
- **Cleaner spec.** The `lance/` extension has exactly two entries: `metadata.json` and `docs.lance/`. No optional sub-files.
- **Portable Arrow format.** The `docs.lance/` directory can be inspected with any Arrow reader (Python, Rust, Polars, DuckDB, etc.) without special tooling.
- **Simpler dependency tree.** `rusqlite` (which pulled in a full C SQLite build via the `bundled` feature) is removed.

### Negative / Trade-offs

- **No semantic (vector) search.** QMD produced embedding vectors for semantic similarity search. LanceDB supports vector search but we are not generating embeddings at index time. Keyword BM25 search is the only mode in 1.2.0. Vector search can be added later (see §Future Work below).
- **Larger binary.** `lancedb` and its Arrow dependencies add ~15–20 MB to the `sempkg` and `SemBundle` binaries compared to `rusqlite`.
- **Async tokio required.** The lancedb Rust API is fully async. `SemBundle build` and `sempkg` are synchronous entry points; both now create a `tokio::Runtime` with `block_on` for the Lance calls. This is idiomatic but adds a runtime.
- **Bundles are not backward-compatible.** Bundles built with spec 1.1.0 (QMD extension) will not provide doc search in `sempkg` 1.2.0+. `sempkg` detects the `lance` extension string and reports no docs index for 1.1.0 bundles.

---

## Migration Notes

### For bundle publishers

1. Upgrade `SemBundle` to the current version.
2. Replace `SemBundle pack --qmd-dir <dir>` with `SemBundle build --docs-dir <dir>` (or pass `--lance-dir` to `SemBundle pack` if you have a pre-built LanceDB directory).
3. Remove the `--qmd-collection-name`, `--qmd-glob`, `--qmd-chunk-strategy` flags — the new `--docs-glob` accepts a comma-separated glob list.
4. Republish bundles. Consumers running `sempkg sync` will pick up the new bundles.

### For bundle consumers

No action required if you only run `sempkg sync` / `sempkg docs`. The `sempkg` binary handles the `lance/` extension transparently. Old 1.1.0 bundles simply show no doc index.

### For `sempkg` Python users

Install the updated package (`uv pip install -e .`). The `lancedb` Python package is now a hard dependency. Old bundle directories with `qmd/` sub-directories will show "(no LanceDB documentation index in this bundle)" in doc search until republished.

---

## Future Work

- **Vector search (ADR-002):** Reintroduce semantic similarity search by adding an `embeddings` column to the LanceDB `docs` table and storing sentence-transformer or local GGUF embedding vectors. LanceDB supports both ANN vector search and hybrid BM25+vector reranking natively.
- **Incremental index updates:** LanceDB supports append/delete operations. A future `sempkg pkg lance-update` command could update only changed files without full reindex.
- **Multi-table bundles:** Additional tables (e.g. `api_reference`, `examples`) could be added to the same LanceDB directory to support structured querying beyond prose search.

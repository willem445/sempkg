# QMD-Inspired Indexing Improvement Backlog

**Status:** Draft
**Date:** 2026-06-19
**Scope:** sempkg documentation indexing, retrieval quality, and MCP ergonomics

---

## Why this exists

This backlog captures the pieces from QMD-style indexing and search that sempkg does not yet fully cover, or only covers in a simpler form.

The current sempkg baseline is already strong:

- package-scoped CodeGraph symbol search
- LanceDB BM25 documentation search
- optional local reranking
- version-pinned bundle installation and MCP exposure

The main opportunity is to improve recall, chunk quality, and search transparency so the retrieval layer behaves more like a polished knowledge system and less like a raw index.

---

## Priority 1: Add semantic/vector retrieval for docs

### Problem

BM25 is good for exact wording, but it misses paraphrases, synonyms, and cross-lingual queries. QMD’s vector search gives it a major quality boost for queries that do not share vocabulary with the source text.

### What to add

- Add an embeddings column to the docs LanceDB table.
- Support vector search and hybrid BM25 + vector retrieval.
- Keep BM25 as the default fast path.
- Gate semantic search behind an explicit config flag so it remains optional.

### Why it helps

- Better recall for natural-language questions.
- Better results on docs with domain-specific jargon.
- More resilient search when users do not know the exact API wording.

### Success criteria

- `sempkg docs` can return vector-ranked results.
- Hybrid retrieval improves top-k quality on synonym-heavy queries.
- Existing BM25 behavior remains available and unchanged by default.

---

## Priority 2: Add structured context metadata

### Problem

QMD’s context tree model helps the search engine understand where a document lives and how it relates to nearby content. sempkg currently scopes by package, but it does not yet have a comparable path-level context layer.

### What to add

- Allow optional context metadata on indexed docs.
- Support hierarchical context such as package, subdirectory, and document-family notes.
- Surface context in search output and MCP metadata.

### Why it helps

- Disambiguates repeated names across large corpora.
- Makes package docs feel more navigable.
- Gives agents more grounding when a query depends on project structure.

### Success criteria

- Search results can show context breadcrumbs.
- Context can be indexed without changing bundle semantics.
- Retrieval quality improves on ambiguous package layouts.

---

## Priority 3: Improve chunking quality

### Problem

Chunk boundaries strongly affect search quality. QMD gets value from markdown-aware and AST-aware chunking; sempkg can gain the same benefit if it stops relying on coarse text splitting alone.

### What to add

- Markdown-aware chunking for docs.
- Symbol-aware chunking for code and source excerpts.
- Optional AST-aware chunking for supported languages.
- Preserve line and byte offsets so citations stay precise.

### Why it helps

- Better chunk coherence.
- Fewer “half a thought” results.
- Better reranking because each candidate represents a more complete semantic unit.

### Success criteria

- Chunks align with document structure instead of arbitrary text windows.
- Search results remain citeable back to a stable source span.
- Oversized chunks degrade gracefully instead of being dropped.

---

## Priority 4: Add query expansion and intent hints

### Problem

QMD benefits from query expansion, structured search inputs, and an intent hint. sempkg’s current retrieval is more direct and less adaptive.

### What to add

- Optional query expansion before retrieval.
- An `intent` hint for broad disambiguation.
- Structured query inputs for advanced callers.

### Why it helps

- Improves lexical recall before reranking.
- Helps the engine distinguish between similarly named APIs.
- Gives power users more control without forcing it on everyone.

### Success criteria

- Simple queries still work unchanged.
- Expanded queries increase top-k recall on ambiguous searches.
- The MCP surface can accept intent without breaking existing callers.

---

## Priority 5: Add index diagnostics and freshness checks

### Problem

As soon as the retrieval surface grows, users need a way to tell whether a bundle is stale, misconfigured, or missing expected index pieces.

### What to add

- A doctor/status command for docs indexes.
- Freshness checks for embeddings and index metadata.
- Clear warnings for missing tables, missing FTS, or mismatched config.

### Why it helps

- Faster debugging.
- Easier bundle support.
- Better confidence in reproducibility.

### Success criteria

- Users can tell whether a bundle is healthy in one command.
- Common indexing mistakes are reported clearly.
- Status output stays small but actionable.

---

## Priority 6: Improve retrieval transparency

### Problem

When search quality changes, it helps to see why a result won. QMD has richer explainability in some search paths; sempkg can expose more of that without changing core behavior.

### What to add

- Score traces for docs search.
- Source type labels such as symbol, doc chunk, or code chunk.
- Optional explain mode in MCP and CLI output.

### Why it helps

- Makes debugging ranking issues easier.
- Gives agents more trust in the returned evidence.
- Helps tune hybrid retrieval and reranking.

### Success criteria

- Users can inspect how a result was ranked.
- Explanations do not overwhelm default output.
- Explain mode remains optional.

---

## Suggested implementation order

1. Vector retrieval for docs.
2. Chunking improvements.
3. Context metadata.
4. Query expansion and intent hints.
5. Diagnostics and freshness checks.
6. Retrieval transparency.

This ordering prioritizes measurable search quality gains first, then usability and maintainability.

---

## Notes on scope

- Keep BM25 as the default path.
- Keep all new behavior opt-in unless the quality win is clearly safe.
- Preserve version pinning and bundle isolation.
- Prefer additive changes to the bundle spec unless a feature truly requires a format change.

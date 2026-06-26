# ADR-002: Hybrid Vector + BM25 Search with Query Expansion

**Date:** 2026-06-15
**Status:** Accepted
**Deciders:** sempkg maintainers

---

## Context

The MCP `query` tool fused only **BM25 (full-text)** signals across the code,
docs, and CodeGraph indexes, ranked them with Reciprocal Rank Fusion (RRF), and
optionally reranked the pool with a local Qwen3-Reranker GGUF model
(see ADR-001 for the LanceDB indexing foundation).

Pure lexical retrieval misses semantically relevant results that do not share
surface tokens with the query (synonyms, paraphrases, "how do I …" questions).
[QMD](https://github.com/tobi/qmd) demonstrates a stronger recipe for docs:
generative **query expansion** plus parallel **vector** and BM25 search fused
with RRF. We want the same recall benefit for *both* code and docs, while
keeping the tool fully functional when the extra models are absent.

## Decision

Add an optional **hybrid retrieval** stage in front of the existing reranker:

1. **Query expansion** — a fine-tuned `qmd-query-expansion-1.7B` GGUF model
   rewrites the query into typed sub-queries. `lex:` variants route to BM25;
   `vec:`/`hyde:` variants route to vector search. The original query always
   runs against **both** backends with double RRF weight.
2. **Vector search** — document/code chunks are embedded with
   `Qwen3-Embedding-0.6B` (1024-dim, L2-normalized) and stored in a `vector`
   `FixedSizeList<Float32>` column added to the existing LanceDB tables. Queries
   are embedded with the same model (Qwen3 instruct prefix) and searched via
   LanceDB cosine kNN.
3. **RRF fusion** — every (run × backend × source) hit contributes
   `weight / (60 + rank)`; duplicates are summed so multi-signal agreement
   boosts ranking. The fused pool then flows into the unchanged diversity →
   small-to-big → two-pass reranking stages.

### Where embeddings are generated

Embeddings are produced at the **sempkg** level (`sempkg embed`), not at
`sembundle` build time. This keeps `.sembundle` archives small and model-neutral
and lets each workspace opt in with the model it prefers. The embedding model
id + dimension are stamped into each table's `metadata.json`; vector search is
skipped for any table whose stored model does not match the configured query
embedder.

### Build feature

Both models run via `llama-cpp-2` behind a new `embeddings` cargo feature,
independent of the existing `reranker` feature (they can be enabled together).

## Consequences

- **Graceful degradation (required):** if a model is missing, the `embeddings`
  feature is not compiled, or a bundle has no compatible vectors, the `query`
  tool falls back to BM25-only retrieval — and to RRF-only ranking when the
  reranker is absent. The CLI and MCP server always start.
- **Storage:** the `vector` column adds ~`4 × dim` bytes per chunk. Full source
  bodies were already stored for symbols, so the marginal cost is modest.
- **Compatibility:** existing bundles without embeddings keep working unchanged;
  `embedding_model` / `embedding_dim` metadata fields are optional and default
  to `None`.
- **New commands:** `sempkg embed`, `sempkg embedding pull|status`, and
  `sempkg query-expansion pull|status|test`; new `[embedding]` and
  `[query_expansion]` sections in `sempkg.toml`.

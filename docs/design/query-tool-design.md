# Query Tool Design Note

**Date:** 2026-06-24
**Status:** Implemented
**Scope:** `src/sempkg/src/mcp.rs` — `tool_query` method and `UnifiedHit` type

---

## 1. Overview

The `query` MCP tool is a single cross-package entry point for natural-language or keyword
searches.  An agent submits a free-form question (e.g. *"Where does ADC sampling happen?"*) and
receives ranked results drawn from every installed bundle and registered local package, without
needing to know which package to target first.

Internally the tool:
1. fans out to all available retrieval backends across all packages,
2. deduplicates hits that refer to the same symbol (codegraph and code-index often return the same function),
3. merges the deduplicated result sets with Reciprocal Rank Fusion,
4. applies a diversity cap to guarantee balanced source representation,
5. **expands** each pool hit to its full symbol body (small-to-big retrieval),
6. scores the resulting pool with the local Qwen3 reranker (when loaded), and
7. returns rich markdown annotated with package provenance, score, file, lines, and a snippet.

---

## 2. Retrieval Fan-out

For each installed bundle and local package the tool queries whichever backends that package
exposes:

| Backend | Condition | Description |
|---------|-----------|-------------|
| **Code index** (`lance/code`) | bundle built with `--include-source` | BM25 full-text search over symbol bodies; returns symbol, kind, signature, and a source snippet |
| **Docs index** (`lance/docs`) | bundle built with `--include-lance` | BM25 full-text search over documentation chunks |
| **CodeGraph** (`graph/codegraph.db`) | any indexed package or bundle | SQLite FTS symbol search returning name, kind, signature, and file location |

Each backend is queried for `fetch_limit` candidates (`max(top_k, 20)` where `top_k` comes from
`[reranker] top_k` in `sempkg.toml`).

---

## 3. Deduplication

CodeGraph and the LanceDB code index are built from the same source corpus.  A function
`process_adc` will therefore appear in both: once as a structured symbol record (codegraph) and
again as a snippet-carrying entry in the code index.  Without deduplication, both hits consume
slots in the reranker pool and the reranker sees redundant content.

### 3.1 Key construction

After collection, each hit is assigned a **dedup key** before any sorting occurs:

| Origin | Key formula |
|--------|-------------|
| `code` / `codegraph` | `package:normalise(path):start_line` — or `package:normalise(path):symbol_lowercase` if line is unknown |
| `docs` | `package:normalise(path):hex(hash(snippet))` — content hash distinguishes distinct chunks; identical content collapses regardless of position |

Two design decisions are worth noting explicitly:

**Package prefix on every key.** An earlier version omitted the package component, which meant
`src/lib.rs:42` in bundle A and `src/lib.rs:42` in bundle B would silently merge.  That
undermined the cross-package RRF and diversity-cap work whose entire purpose is to guarantee
representation from every package.  The package name is now always the first segment.

**Content hash for docs instead of line numbers.** LanceDB doc chunks carry no line-number
metadata — `start_line` and `end_line` are always 0.  The old `path:0:0` key collapsed every
chunk of a document onto the same slot; only the first-seen chunk survived, and it was not
necessarily the most relevant one.  Hashing the snippet content gives each distinct chunk its
own bucket while still collapsing byte-for-byte duplicates (the same doc indexed in two bundles).

Path normalisation lowercases the string and converts `\` to `/` so Windows-style codegraph
paths (`src\adc\sampling.rs`) match the forward-slash paths stored in the lance index.

### 3.2 Collision resolution

On a key collision the **richer** hit wins:

```
origin_priority:  code (2)  >  codegraph (1)  >  docs (0)
tiebreaker:       longer snippet wins within the same priority
```

After selecting the winner, `merge_complementary` fills any gaps the winner is missing by
harvesting from the loser:

- **Symbol name** — the longer (more qualified) name is kept; codegraph typically holds the
  fully-qualified name while the code index may store only the short name.
- **Signature / kind** — filled from the loser when the winner has an empty field.
- **Line range** — donor's non-zero values replace winner's zeros.

The net result for a typical code/codegraph collision: the `code` hit wins (carries the source
body), but retains the qualified symbol name and accurate line range from the codegraph record.

### 3.3 RRF score on collision

The `rrf_score` carried by the surviving entry is set to `max(winner.rrf_score, loser.rrf_score)`
in **both** collision branches.  This matters because richness (payload quality) and retrieval
rank (relevance signal) are independent: a codegraph hit that ranked 1st in its own source list
(high RRF) may lose the richness comparison to a code-index hit that ranked 10th.  Discarding
the stronger fusion signal would penalise the merged entry during the subsequent global sort and
diversity selection.  Taking the maximum ensures that agreement between two sources can only
raise a hit's pool-selection priority, never lower it.

---

## 4. Reciprocal Rank Fusion

Results from the three backends arrive on incompatible score scales: CodeGraph uses SQLite FTS
BM25, the LanceDB code index uses LanceDB BM25, and the docs index uses LanceDB BM25 with
different corpus statistics.  Mixing raw scores would systematically favour whichever backend
happens to return the highest absolute values.

All hits are instead assigned a uniform **RRF score**:

$$
\text{rrf}(d) = \frac{1}{k + \text{rank}(d)}
$$

where $k = 60$ (the standard Cormack & Clarke constant) and $\text{rank}(d)$ is the 1-based
position of the result within its own source's ranked list.  This maps every backend onto the
same scale — rank-1 from code, docs, and codegraph are all equally valued at $1/61 \approx 0.016$.

After scoring, all hits from all packages are sorted globally by `rrf_score` descending.

---

## 5. Diversity Selection

A single large package with all three backends active could still fill the reranker's `top_k`
pool before smaller packages or less-used origins contribute anything.

After the RRF sort a **greedy diversity pass** enforces a per-`(package, origin)` bucket cap:

```
max_per_bucket = pool_size / 3   (minimum 3)
```

The pass iterates through the RRF-sorted list and accepts each hit into the pool unless its
bucket is already full, stopping once `pool_size` candidates have been selected.  With three
origins and a typical `pool_size` of 20 this gives each origin up to ~6 slots per package,
preventing any single source from monopolising the pool the reranker will actually score.

---

## 6. Small-to-big Retrieval Expansion

BM25 retrieval returns a truncated display snippet (600 chars for code, 400 for docs) because
storing and transferring the full symbol body for every candidate in the fan-out pool would be
unworkable.  However, the reranker is a cross-encoder that reads the full `(query, document)`
pair character by character — sending it only 600 characters of a 2 KB function body means it
judges relevance on a partial view of the evidence.

After the diversity selection step has committed to a small pool (typically 10–20 hits), the
tool runs a **small-to-big expansion pass** that replaces each hit's truncated snippet with the
complete symbol body fetched via a precise key lookup into the code index:

```
BM25 retrieval (fine-grained)     ─► small candidate pool
                                       │
               expand_pool_hits ──────►│ fetch full body for each code/codegraph hit
                                       │
              Qwen3-Reranker (big) ────►│ score (query, full_body) pair
```

### 6.1 Lookup strategy

For each pool hit the expansion function (`McpContext::expand_pool_hits`) attempts two lookups
in priority order:

1. **Location-keyed lookup** — `lance::fetch_symbol_at_location(code_dir, path, start_line)`.
   Uses the file path and start line recorded in the hit to retrieve the exact `SymbolSource`
   row from the code index.  This is the primary path for `code` origin hits because the
   location is embedded in the index at bundle-build time.

2. **Name-keyed lookup (fallback)** — `lance::fetch_symbol_source(code_dir, symbol)`.  Used
   when `start_line == 0` (common for `codegraph` hits that were not matched to the code index
   during dedup) or when the location lookup returns nothing.  Only fires if the match is
   unambiguous (`SymbolLookup::Unique`); ambiguous matches are left unexpanded to avoid
   returning the wrong function body.

### 6.2 Expansion guard

The expansion is written only when `body.len() > snippet.len()`.  This ensures the expansion
never silently replaces a richer snippet with a shorter one (e.g. when the code index stores a
one-liner whose signature _is_ the full body).

### 6.3 What is expanded and what is not

| Origin | Expanded? | Rationale |
|--------|-----------|-----------|
| `code` | Yes | Primary target; symbol bodies with leading/trailing comments are stored in the code index |
| `codegraph` | Yes (when a code index is available for the package) | Symbol bodies are available via the shared code index; codegraph itself stores only signatures |
| `docs` | **No** | Documentation chunks are already the natural retrieval unit; there is no larger parent entity to expand into |

### 6.4 Display vs. reranker input

The `snippet` field on `UnifiedHit` is **never mutated** during expansion.  The `expanded_text`
field is stored separately and consumed only by `unified_hit_candidate_text` when building the
reranker input string.  The output markdown always shows the original display snippet.  Agents
that need the full body can call the `read_code` tool with the path and line numbers that are
already present in every result.

---

## 7. Reranking

The diversity-selected pool (up to `pool_size` candidates) is submitted to the local
Qwen3-Reranker cross-encoder (see [`reranker-design.md`](reranker-design.md)).  The reranker
scores each `(query, candidate_text)` pair and returns results sorted by descending relevance.
The top `limit` results (default 10, configurable per call) are kept.

When the reranker model is not loaded (feature disabled or model not yet downloaded) the tool
falls back to returning the top `limit` hits from the diversity-selected pool, scored by RRF.

---

## 8. Relevance Floor

When the reranker is active a relevance floor of **0.10** is applied: hits whose reranker score
falls below this threshold are dropped from the output even if they would otherwise rank within
the top `limit`.  This prevents the tool from surfacing syntactically matching but semantically
irrelevant results to the agent.

The floor is not applied in fallback (RRF-only) mode, where no calibrated relevance signal
exists.

---

## 9. Output Format

Each result is rendered as a markdown section containing:

- **Heading** — rank number, symbol name or path, kind, and reranker score
- **Metadata table** — package, origin (code / docs / codegraph), source file, line range
- **Signature** — if present (code and codegraph hits)
- **Snippet** — fenced code block with the relevant excerpt

Results are separated by `---` dividers so agents can parse individual sections.

---

## 10. Pipeline Summary

```
query string
     │
     ├─► lance::search_code  (per package with code index)  ──┐
     ├─► lance::search       (per package with docs index)  ──┤  RRF score
     └─► codegraph::query    (per indexed package)          ──┘  1/(60+rank)
                                        │
                              dedup  O(n) HashMap pass
                              key = package:normalise(path):start_line
                              code > codegraph > docs  (richness)
                              merge_complementary fills gaps in winner
                                        │
                              global sort by rrf_score
                                        │
                              greedy diversity selection
                              per-(package,origin) cap = pool_size/3
                                        │
                              small-to-big expansion
                              fetch full symbol body from code index
                              location-keyed → name-keyed fallback
                              docs hits skipped (already natural unit)
                                        │
                              Qwen3-Reranker
                              score each (query, expanded_text|snippet) pair
                                        │
                              relevance floor 0.10 (reranker mode only)
                                        │
                              top limit results
                              formatted as markdown
```

---

## 11. Future Directions

The query tool is designed to be a stable insertion point for improved retrieval.  Planned
enhancements (tracked in the roadmap):

- **Query expansion** — a local LLM generates sub-queries or synonym expansions before fan-out
- **Parallel vector search** — dense embedding search alongside BM25, merged via RRF before the
  diversity step
- **Parallel BM25 + vector per source** — for each backend, run both lexical and semantic search
  and fuse within-source before the cross-source merge
- **Configurable bucket caps** — expose `max_per_bucket` and `pool_size` in `sempkg.toml` under
  a `[query]` section so operators can tune diversity vs. recall trade-offs per deployment

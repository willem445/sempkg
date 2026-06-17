# Reranker Design Note

**Date:** 2026-06-16
**Status:** In progress
**Scope:** `src/sempkg/src/reranker.rs` and its integration with the MCP server

---

## 1. Overview

The reranker is an optional, locally-executed second-pass scoring layer that sits between the
primary retrieval backends (LanceDB BM25 and CodeGraph symbol search) and the results returned
to the agent.

It is gated behind the `reranker` Cargo feature so that the default binary carries no inference
dependency.  When enabled it loads a quantised GGUF cross-encoder model via **llama-cpp-2**
(utilityai/llama-cpp-rs) and scores every candidate against the query on the user's own hardware.
No data leaves the machine; no API key is required.

---

## 2. Current Architecture

### 2.1 Retrieval pipeline

```
User query
    │
    ├─► CodeGraph symbol search  ─────────┐
    │   (regex / prefix, ranked by kind)  │
    │                                     ▼
    └─► LanceDB BM25 doc search  ──► union pool (up to top_k=20 candidates)
                                         │
                                         ▼
                                   Qwen3-Reranker
                                   (cross-encoder, RANK pooling)
                                         │
                                         ▼
                                   output_n=5 results
                                   sorted by P(yes)
```

In the MCP server (`mcp.rs`) both `search_symbols` and `search_docs` produce candidates, convert
them to `RerankCandidate` via `codegraph_json_to_candidates` / `lance_results_to_candidates`, and
pass the union to `Reranker::rerank`.

### 2.2 Model

| Property | Value |
|---|---|
| Model | `ggml-org/Qwen3-Reranker-0.6B-Q8_0-GGUF` |
| File | `qwen3-reranker-0.6b-q8_0.gguf` (~604 MiB) |
| Architecture | Qwen2 decoder (causal, `causal_attn=1`) |
| Pooling | `RANK` (`pooling_type=4` in GGUF metadata) |
| Classifier head | 2-class: `cls_label[0]=yes`, `cls_label[1]=no`, `n_cls_out=2` |
| Tokeniser | Embedded in GGUF (GPT-2/Qwen2 tokeniser, no external file needed) |
| Backend | llama-cpp-2 v0.1 (wraps llama.cpp) |
| Build requirement | `libclang` (bindgen) — set `LIBCLANG_PATH` on Windows |

### 2.3 Scoring mechanics

For each (query, document) pair:

1. Build the Qwen3-Reranker chat prompt:

   ```
   <|im_start|>system
   Judge whether the Document meets the requirements of the Query.
   Note that the answer can only be "yes" or "no".<|im_end|>
   <|im_start|>user
   <Instruct>: Given a search query, retrieve relevant code and documentation…
   <Query>: {query}
   <Document>: {document}<|im_end|>
   <|im_start|>assistant
   <think>

   </think>
   ```

2. Tokenise with BOS, truncate to `n_ctx=4096`.
3. Run `ctx.decode()` — a causal forward pass (decoder model; `encode()` is for
   encoder-decoder models like T5 and would produce wrong results).
4. Read `embeddings_seq_ith(0)`.  RANK pooling on the 2-class head returns softmax
   probabilities.  `emb[0]` is P(yes).  The remaining 1022 slots are unused buffer
   fill and can be ignored.
5. Return `emb[0]` directly as the relevance score — no sigmoid.  (Applying sigmoid to
   a probability would compress values toward 0.5 and destroy discrimination.)

### 2.4 Current limitations

- **One context per pair.**  A fresh `LlamaContext` is created and destroyed for each
  candidate.  This is correct but allocates ~448 MiB KV cache and ~302 MiB compute
  buffer on every call.  Context creation from an already-loaded model is fast in
  practice, but it does prevent batching.

- **Pointwise only.**  Each candidate is scored independently.  The model has no view
  of how a document compares to the others in the pool.

- **Single retrieval source per tool call.**  `search_symbols` and `search_docs` run
  their own BM25 / graph queries and only rerank their own results.  There is no
  cross-source fusion step.

- **No query understanding before retrieval.**  The user's raw query is sent as-is to
  both BM25 and the reranker.  Ambiguous or terse queries get no expansion.

---

## 3. Planned Improvements

### 3.1 Query expansion (multi-hypothesis retrieval)

**Motivation.**  BM25 is a lexical matcher: a query for `"async cancellation"` will miss
`"task abort"` even if a document is semantically identical.  The reranker can correct
false positives but cannot recover true positives that never entered the candidate pool.
Expanding recall before reranking is therefore more valuable than improving reranker
precision alone.

**Design (mirror QMD's query expansion pattern).**

```
original query
    │
    ▼
Query Expander (fast)
    │  generates N alternative phrasings / hypothetical doc snippets
    │
    ├─► phrasing_1 ──► BM25 retrieval (top_k/N candidates)
    ├─► phrasing_2 ──► BM25 retrieval (top_k/N candidates)
    │   …
    └─► phrasing_N ──► BM25 retrieval (top_k/N candidates)
                              │
                              ▼
                    deduplicate by source path
                              │
                              ▼
                      Qwen3-Reranker scores
                      each candidate against
                      the **original** query
                              │
                              ▼
                         output_n results
```

**Expansion options (ascending cost):**

| Strategy | Cost | Quality |
|---|---|---|
| Synonym/abbreviation table (hand-curated Rust/code terms) | Zero inference | Low |
| HyDE — ask an LLM to write a hypothetical answer, embed that | 1 LLM call | High |
| Multi-query — ask an LLM for N paraphrases | 1 LLM call | Medium-high |
| Sub-queries — decompose compound questions | 1 LLM call | High for long queries |

For `sempkg` the most practical starting point is **multi-query via the host MCP
client**: the MCP server already runs inside the agent session, so it can call back to
the agent to produce paraphrases before running retrieval.  This avoids bundling a
second inference model.

Alternatively, a compact local model (e.g. `smollm2-135m.gguf`, ~100 MiB) can be
loaded on the same `LlamaBackend` as the reranker and used for expansion only.

**`RerankerConfig` additions needed:**

```toml
[reranker]
query_expansion = true         # off by default
expansion_count = 3            # number of alternative phrasings
expansion_model = "~/.sempkg/models/smollm2-135m.gguf"  # optional
```

---

### 3.2 Multi-source fusion via Reciprocal Rank Fusion (RRF)

**Motivation.**  `search_symbols` and `search_docs` currently run in isolation.  A
query that touches both API symbols and prose documentation will get two independent
ranked lists; the agent sees them separately and must mentally merge them.  A single
fused, reranked list is more useful.

**RRF formula:**

$$\text{score}_\text{RRF}(d) = \sum_{i=1}^{L} \frac{1}{k + \text{rank}_i(d)}$$

where $L$ is the number of ranked lists, $\text{rank}_i(d)$ is the 1-based position of
document $d$ in list $i$ (or $\infty$ if absent), and $k=60$ is the standard constant
that dampens the advantage of the top position.

RRF is parameter-free (only $k$ needs tuning), robust to score-scale differences between
heterogeneous sources, and competitive with learned fusion at retrieval scales below
~10 K documents.

**Pipeline:**

```
query
 ├─► CodeGraph symbols  ──► ranked list A  ──┐
 ├─► LanceDB BM25 docs  ──► ranked list B  ──┤
 └─► (future) vector search ──► list C  ──┬─┘
                                          │
                                     RRF fusion
                                          │
                               unified pool (top_k candidates)
                                          │
                                    Qwen3-Reranker
                                          │
                                     output_n results
```

**`search_all` MCP tool.**  RRF fusion motivates a new `search_all` tool that runs all
backends simultaneously and returns a single merged list.  The existing `search_symbols`
and `search_docs` tools remain for callers that want source-separated results.

**`RerankerConfig` additions needed:**

```toml
[reranker]
rrf_k = 60      # RRF constant; 60 is the standard default
fusion = true   # enable RRF pre-fusion before reranking
```

---

### 3.3 Position-aware blend output

**Motivation.**  The reranker is a 0.6B model scoring documents in isolation.  On
ambiguous or very short queries it can produce flat, uninformative scores (many
candidates near 0.1–0.3 with little spread).  In this regime, discarding the original
retrieval rank entirely in favour of the neural score introduces noise.

A **position-aware blend** preserves the original rank signal as a regulariser:

$$\text{score}_\text{final}(d) = \alpha \cdot s_\text{reranker}(d) + (1-\alpha) \cdot \text{decay}(\text{rank}_\text{retrieval}(d))$$

where $\text{decay}(r) = 1 / \log_2(1 + r)$ (NDCG-style discount) maps retrieval
rank $r$ to $[0,1]$.

**Effect:**

| Scenario | Behaviour |
|---|---|
| Reranker confident (spread > 0.3) | Neural score dominates; retrieval rank matters little |
| Reranker uncertain (spread < 0.05) | Position discount acts as tie-breaker; avoids random reordering |
| BM25 and reranker agree | Scores reinforce; top item floats clearly above the rest |
| BM25 and reranker disagree | Blend is a compromise; neither signal fully overrides |

**Adaptive α.**  Rather than a fixed weight, $\alpha$ can be computed from the spread of
reranker scores in the pool:

$$\alpha = \min\left(1,\ \frac{\sigma(s_\text{reranker})}{\sigma_\text{min}}\right)$$

When the reranker score distribution is flat ($\sigma$ small), $\alpha$ is low and
retrieval rank governs.  When the distribution is spread ($\sigma$ large), the neural
scores take full control.

**`RerankerConfig` additions needed:**

```toml
[reranker]
blend_alpha = 0.8               # fixed weight; ignored if adaptive_blend = true
adaptive_blend = true           # compute alpha from score spread
adaptive_blend_sigma_min = 0.1  # spread threshold below which retrieval rank takes over
```

---

## 4. Interaction Between the Three Improvements

The improvements are orthogonal and compose naturally:

```
query
 │
 ▼
[3.1] query expansion → N candidate sets
 │
 ▼
[3.2] RRF fusion across sources × query variants → unified pool
 │
 ▼
Qwen3-Reranker scores each candidate against original query
 │
 ▼
[3.3] position-aware blend of neural score + RRF rank
 │
 ▼
output_n final results
```

Implemented together this pipeline mirrors the architecture of production neural search
systems (e.g. Cohere Rerank, Voyage AI) while running entirely on local hardware.

---

## 5. Known Constraints and Trade-offs

| Concern | Detail |
|---|---|
| **Per-pair context cost** | Each `score_pair` call allocates ~750 MiB of KV+compute buffer. Batching multiple pairs into one decode pass would require keeping the context alive between calls, which is at odds with `Reranker` holding `LlamaBackend` and `LlamaModel` by value (not `Arc`). A future refactor could use a `OnceLock<LlamaContext>` cached between calls with manual KV-cache clearing. |
| **Context window** | At 4096 tokens, very long documents (e.g. full Markdown pages) are silently truncated. A chunking pass before reranking — scoring the best chunk of each document rather than the whole text — would improve accuracy on large documents. |
| **Score calibration** | P(yes) values from this model tend to be low (~0.2–0.4) for genuinely relevant code. This is expected (the model is conservative) and does not affect ranking quality, but it may confuse users who expect scores near 1.0 for good matches. The score label already says "1.0 = highly relevant" — consider adding a note that scores above 0.3 are strong. |
| **llama-cpp-2 build requirement** | Requires `libclang` (bindgen) at compile time on all platforms. On Windows: `$env:LIBCLANG_PATH = "C:\Program Files\LLVM\bin"` after `winget install LLVM.LLVM`. This is documented in the DEV-GUIDE but not yet in CI. |
| **lancedb `half` pin** | `lancedb 0.14` pins `half = 2.4.1`. Any future embedding model dependency that also needs `half` must satisfy this constraint. |

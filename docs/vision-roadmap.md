# **sempkg — Vision & Roadmap**

## **Vision**

**sempkg aims to become the universal semantic index registry and multi-repo intelligence layer for agentic coding systems.**
It extends CodeGraph from a single-repo indexer into a **multi-version, multi-repository, zero-source-code semantic graph server** that tools, agents, and IDEs can query through a unified MCP interface.

The long-term goal is to eliminate the need for developers to clone or locally store massive external codebases.
Instead, codebases will publish **prebuilt semantic index bundles** alongside their releases, and sempkg will dynamically install, version, and serve them.

This transforms CodeGraph from a local analysis tool into a **package manager for code intelligence**.

---

# **Core Principles**

- **No source code required after indexing**
  CodeGraph's graph store contains all semantic information needed for navigation, search, and reasoning.

- **Portable, versioned semantic bundles**
  Codebases can ship .sembundle archives with each tag or release.

- **Dynamic multi-graph switching**
  Projects can pin specific dependency versions, and sempkg can install and serve graphs on demand.

- **Unified MCP server**
  A single server exposes all indexed codebases and versions through a consistent tool interface.

- **Composable semantic memory**
  Multiple graphs can be mounted simultaneously to create a virtual-monorepo view for agents.

- **Local LLM reranking (no cloud required)**
  Query results from both CodeGraph symbol search and LanceDB documentation search can be reranked
  by a locally running GGUF model, similar to how QMD used embedding-based reranking.
  No API keys, no data leaving the machine.

---

# **Current Capabilities**

- Install bundles from a self-hosted or remote registry into workspace-local or global stores
- Serve all installed graphs and documentation indexes through a single MCP server (sempkg mcp)
- Provide symbol search, call graph queries, and LanceDB BM25 documentation search
- Register and index locally cloned repositories with CodeGraph
- Manage bundle dependencies via `sempkg.toml` and `sempkg.lock`

---

# **Roadmap**

## **1. Portable SemBundle Format**

Define a portable artifact format:

`
my-sdk-1.2.3.sembundle
`

Containing:

`
graph/
embeddings/
metadata.json
config.json
lance/          (optional -- LanceDB documentation index)
`

### Goals

- Allow codebases to publish bundles with each release
- Enable sempkg to download and mount bundles without cloning repos
- Support checksum verification, Ed25519 signing, and caching

---

## **2. Remote Bundle Registry**

Introduce a registry system:

`
registry/
  qt/6.7.0.sembundle
  ros2/humble.sembundle
  aws-sdk/1.11.210.sembundle
`

### Goals

- Support local, remote, and mirrored registries
- Allow organisations to host private bundle registries
- Enable automatic updates when new versions are released

---

## **3. Version Pinning and Project Manifests**

Add per-project configuration:

```toml
# sempkg.toml
[dependencies]
aws-sdk = "1.11.210"
qt      = "6.7.0"
```

### Goals

- Reproducible installs via `sempkg.lock`
- Automatically install the correct graph versions
- Support optional dependency groups

---

## **4. Dynamic Graph Loading and Unloading**

Enable sempkg to:

- Install graphs on demand
- Unload unused graphs from memory
- Cache recently used graphs
- Mount multiple graphs simultaneously

### Goals

- Reduce memory footprint
- Improve startup time
- Support large dependency sets

---

## **5. Multi-Graph Query Engine**

Extend MCP tools to support:

- `list_packages()`
- `search_symbols(package, query)`
- `search_across(packages[], query)`
- `get_callers` / `get_callees` across multiple packages

### Goals

- Provide a unified semantic view
- Allow agents to reason across dependency boundaries
- Enable virtual-monorepo navigation

---

## **6. Local LLM Reranker for CodeGraph and LanceDB**

Add an optional local LLM reranking stage, similar to what QMD provided via GGUF embedding
models, but applied at query time rather than at index time. This avoids large model downloads
at bundle-build time while still enabling semantic relevance ranking for both search surfaces.

### How it works

1. A first-pass **candidate retrieval** phase runs the existing fast indexes:
   - CodeGraph BM25/FTS symbol search for `search_symbols` / `get_context`
   - LanceDB BM25 full-text search for `search_docs`
2. A second-pass **reranking** phase scores the top-N candidates through a locally running GGUF
   model (e.g. a `nomic-embed` or `bge-reranker` quantised model) via llama.cpp.
3. Results are re-sorted by model score and returned to the MCP caller.

The reranker is **entirely optional** and **zero-cloud**: if no model is configured, both tools
fall back to pure BM25 results unchanged. When enabled, the model runs in-process via a Rust
llama.cpp binding (e.g. the `llm` or `llama-cpp-rs` crate).

### Configuration

```toml
# sempkg.toml
[reranker]
model    = "~/.sempkg/models/bge-reranker-v2-m3-q4_k_m.gguf"
top_k    = 20   # candidates passed to the model
output_n = 5    # final results returned
```

### Goals

- No API keys or internet access required for semantic reranking
- Works offline and in air-gapped environments
- Reranker model downloaded and managed independently of bundles
- Improves relevance for both symbol search and documentation search

---

## **7. IDE and Agent Integrations**

Improve developer experience:

- VS Code extension
- JetBrains plugin
- Copilot / Claude / Cursor integration

### Goals

- Make sempkg the default semantic backend for agentic coding
- Provide instant context for external SDKs and frameworks

---

## **8. Optional: Distributed or Cloud-Hosted Mode**

Future exploration:

- Host graphs in object storage
- Stream graph data on demand
- Provide a hosted SemBundle cloud registry service

### Goals

- Support extremely large codebases
- Enable team-wide shared semantic memory
- Reduce local storage requirements

---

# **Long-Term Vision**

sempkg becomes the **semantic infrastructure layer** for modern software development:

- Codebases publish semantic bundles like they publish binaries
- Agents and IDEs consume these bundles without cloning repos
- Developers gain instant, deep understanding of any dependency
- Local LLM reranking delivers cloud-quality relevance with no data leaving the machine
- Multi-agent systems share a unified, versioned semantic memory

This unlocks a future where code intelligence is **portable, composable, and universal**.

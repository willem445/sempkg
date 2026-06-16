# **Codegraph‑Hub — Vision & Roadmap**

## **Vision**

**Codegraph‑Hub aims to become the universal semantic index registry and multi‑repo intelligence layer for agentic coding systems.**
It extends CodeGraph from a single‑repo indexer into a **multi‑version, multi‑repository, zero‑source‑code semantic graph server** that tools, agents, and IDEs can query through a unified MCP interface.

The long‑term goal is to eliminate the need for developers to clone or locally store massive external codebases.
Instead, codebases will publish **prebuilt semantic index bundles** alongside their releases, and Codegraph‑Hub will dynamically load, version, and serve them.

This transforms CodeGraph from a local analysis tool into a **package manager for code intelligence**.

---

# **Core Principles**

- **No source code required after indexing**
  CodeGraph’s graph store contains all semantic information needed for navigation, search, and reasoning.

- **Portable, versioned semantic bundles**
  Codebases can ship `.codegraph` bundles with each tag or release.

- **Dynamic multi‑graph switching**
  Projects can pin specific dependency versions, and Codegraph‑Hub can load/unload graphs on demand.

- **Unified MCP server**
  A single server exposes all indexed codebases and versions through a consistent tool interface.

- **Composable semantic memory**
  Multiple graphs can be mounted simultaneously to create a “virtual monorepo” view for agents.

---

# **Current Capabilities**

- Index multiple locally cloned repositories
- Serve all indexed graphs through a single MCP server
- Provide semantic search, symbol lookup, and cross‑repo navigation
- Maintain a registry of indexed repos and versions
- Allow agents to query multiple codebases without switching servers

---

# **Roadmap**

## **1. Portable CodeGraph Bundles (SemBundle Format)**

Define a portable artifact format:

```
my-sdk-1.2.3.SemBundle
```

Containing:

```
graph/
embeddings/
metadata.json
config.json
```

### Goals

- Allow codebases to publish bundles with each release
- Enable Codegraph‑Hub to download and mount bundles without cloning repos
- Support checksum verification and caching

---

## **2. Remote Bundle Registry**

Introduce a registry system:

```
registry/
  qt/6.7.0.SemBundle
  ros2/humble.SemBundle
  aws-sdk/1.11.210.SemBundle
```

### Goals

- Support local, remote, and mirrored registries
- Allow organizations to host private bundle registries
- Enable automatic updates when new versions are released

---

## **3. Version Pinning & Project Profiles**

Add per‑project configuration:

```
project.json
{
  "dependencies": {
    "aws-sdk": "1.11.210",
    "qt": "6.7.0"
  }
}
```

### Goals

- Automatically load the correct graph versions
- Allow agents to switch contexts based on workspace
- Support fallback and override rules

---

## **4. Dynamic Graph Loading & Unloading**

Enable Codegraph‑Hub to:

- Load graphs on demand
- Unload unused graphs
- Cache recently used graphs
- Mount multiple graphs simultaneously

### Goals

- Reduce memory footprint
- Improve startup time
- Support large dependency sets

---

## **5. Multi‑Graph Query Engine**

Extend MCP tools to support:

- `list_graphs()`
- `select_graph(name, version)`
- `search_across(graphs[], query)`
- `explain_symbol(symbol_id)`
- `list_callers` / `list_callees` across repos

### Goals

- Provide a unified semantic view
- Allow agents to reason across dependency boundaries
- Enable “virtual monorepo” navigation

---

## **6. IDE & Agent Integrations**

Improve developer experience:

- VS Code extension
- JetBrains plugin
- Copilot / Claude / Cursor integration
- QMD memory‑bank compatibility

### Goals

- Make Codegraph‑Hub the default semantic backend for agentic coding
- Provide instant context for external SDKs and frameworks

---

## **7. Optional: Distributed or Cloud‑Hosted Mode**

Future exploration:

- Host graphs in object storage
- Stream graph data on demand
- Provide a hosted “CodeGraph Cloud” service

### Goals

- Support extremely large codebases
- Enable team‑wide shared semantic memory
- Reduce local storage requirements

---

# **Long‑Term Vision**

Codegraph‑Hub becomes the **semantic infrastructure layer** for modern software development:

- Codebases publish semantic bundles like they publish binaries
- Agents and IDEs consume these bundles without cloning repos
- Developers gain instant, deep understanding of any dependency
- Multi‑agent systems share a unified, versioned semantic memory

This unlocks a future where code intelligence is **portable, composable, and universal**.

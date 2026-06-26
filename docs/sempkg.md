# sempkg — User Guide

`sempkg` is a native Rust CLI and MCP server for managing
[SemBundle](sembundle-spec.md) semantic index packages.

It combines [CodeGraph](https://github.com/colbymchenry/codegraph) symbol
intelligence with [QMD](https://github.com/colbymchenry/codegraph)-style
LanceDB documentation search into a single self-contained binary — with an
optional local LLM reranker for high-quality hybrid queries.

---

## Contents

1. [Prerequisites](#prerequisites)
2. [Installation](#installation)
3. [Workspace Setup](#workspace-setup)
4. [Installing Bundles](#installing-bundles)
5. [Indexing a Local Repository](#indexing-a-local-repository)
6. [CodeGraph Queries](#codegraph-queries)
7. [Documentation Search](#documentation-search-lancedb)
8. [Hybrid Query with Reranking](#hybrid-query-with-reranking)
9. [Local Package Management](#local-package-management)
10. [MCP Server](#mcp-server)
11. [Bundle Verification](#bundle-verification)
12. [Full CLI Reference](#full-cli-reference)

---

## Prerequisites

| Requirement | Notes |
|-------------|-------|
| [CodeGraph](https://github.com/colbymchenry/codegraph) | Must be on `PATH`. Install with `npm install -g @colbymchenry/codegraph`. |
| Rust toolchain | Required only when building from source. |

---

## Installation

### Pre-built binaries (recommended)

**Linux / macOS:**
```sh
curl -fsSL https://raw.githubusercontent.com/willem445/codegraph-hub/main/install.sh | sh
```

**Windows (PowerShell):**
```powershell
irm https://raw.githubusercontent.com/willem445/codegraph-hub/main/install.ps1 | iex
```

### Build from source

```powershell
# Standard build
cargo install --path src/sempkg

# With local LLM reranker support (requires a C/C++ compiler for llama.cpp)
cargo install --path src/sempkg --features reranker
```

---

## Workspace Setup

`sempkg` uses a `sempkg.toml` manifest in the project root to declare bundle
dependencies, similar to `Cargo.toml` for Rust crates.

```powershell
cd C:\Projects\my-project
sempkg init
# With a registry pre-configured:
sempkg init --registry https://my-registry.example.com
```

This creates a minimal `sempkg.toml`:

```toml
[workspace]
# verify_key = "keys/publisher.pem"   # optional Ed25519 public key

[[registry]]
name = "default"
url  = "https://registry.example.com"

[dependencies]
```

### Optional reranker configuration

Add a `[reranker]` section to enable hybrid search with the local LLM reranker:

```toml
[reranker]
enabled  = true
# model  = "~/.sempkg/models/Qwen3-Reranker-0.6B-Q8_0.gguf"  # default path
top_k    = 20   # BM25 candidates fed into the reranker
output_n = 5    # final results returned after reranking
```

### Optional vector search + query expansion

The MCP `query` tool runs hybrid retrieval: BM25 (full-text) **and** vector
(semantic) search in parallel, fused with Reciprocal Rank Fusion before the
reranker. Two optional GGUF models power this, both behind the `embeddings`
build feature (`cargo build --features embeddings`):

- **Embedding** (`Qwen3-Embedding-0.6B`) — embeds document chunks and queries
  for vector search.
- **Query expansion** (`qmd-query-expansion-1.7B`) — rewrites the query into
  typed sub-queries (`lex` → BM25, `vec`/`hyde` → vector) for broader recall.

```toml
[embedding]
enabled    = true
# model    = "~/.sempkg/models/qwen3-embedding-0.6b-q8_0.gguf"  # default path
n_ctx      = 2048
gpu_layers = 0    # >0 offloads layers to GPU (needs a GPU llama.cpp build)

[query_expansion]
enabled      = true
# model      = "~/.sempkg/models/qmd-query-expansion-1.7b-q4_k_m.gguf"
max_variants = 4
gpu_layers   = 0
```

Download the models, then build the vector indexes for installed bundles and
local packages:

```bash
sempkg embedding pull          # download the embedding model
sempkg query-expansion pull    # download the query-expansion model
sempkg embed                   # embed docs/code tables (add --force to redo)
sempkg embed <package>         # embed a single package/bundle
```

Status / test helpers:

```bash
sempkg embedding status
sempkg query-expansion status
sempkg query-expansion test "how do I spawn a task"
```

Both models are optional. If a model is missing, the feature is not compiled
in, or a bundle has no embeddings, the `query` tool transparently falls back to
BM25-only retrieval (and to RRF-only ranking when the reranker is absent).

---

## Installing Bundles

Bundles are prebuilt semantic indexes for external libraries, pinned to an
exact version. Add them to `sempkg.toml` and run `sempkg sync` so every team
member installs the same index from the same source.

### From a registry

```powershell
sempkg add aws-sdk@1.11.210
sempkg add requests@2.32.3 --registry my-registry
sempkg sync
```

### From a GitHub release asset (direct URL)

Use `--url` to point directly at a `.sembundle` asset attached to a GitHub
release tag. No registry is needed.

```powershell
# Add to sempkg.toml and install on next sync
sempkg add my-sdk@2.0.0 --url https://github.com/owner/repo/releases/download/v2.0.0/my-sdk-2.0.0.sembundle

# Or install immediately without touching the manifest
sempkg install my-sdk@2.0.0 --url https://github.com/owner/repo/releases/download/v2.0.0/my-sdk-2.0.0.sembundle
```

The URL format for GitHub release assets is always:
```
https://github.com/<owner>/<repo>/releases/download/<tag>/<asset-filename>
```

### GitHub authentication (private / enterprise)

If the repository or release asset is private, or hosted on GitHub Enterprise,
set a token environment variable before running `sempkg add` or
`sempkg install`.

For host `github.company.com`, sempkg checks variables in this order:

1. `GITHUB_TOKEN_GITHUB_COMPANY_COM`
2. `GH_TOKEN_GITHUB_COMPANY_COM`
3. `GITHUB_ENTERPRISE_TOKEN`
4. `GH_ENTERPRISE_TOKEN`
5. `GITHUB_TOKEN`
6. `GH_TOKEN`

Use host-specific variables when possible to avoid mixing public GitHub and
enterprise credentials.

```powershell
$env:GITHUB_TOKEN_GITHUB_COMPANY_COM = "<your-enterprise-pat>"

# Private / enterprise GitHub release URL
sempkg add my-sdk@2.0.0 --url https://github.company.com/owner/repo/releases/download/v2.0.0/my-sdk-2.0.0.sembundle

# Direct source add from a release/tag page
sempkg add https://github.company.com/owner/repo/releases/tag/v2.0.0 --full
```

### Sync options

```powershell
sempkg sync                    # install all [dependencies]
sempkg sync --reinstall        # force reinstall even if already present
sempkg sync --group dev        # also install the "dev" dependency group
sempkg sync --all-groups       # install every dependency group
```

A `sempkg.lock` file is created and updated by `sync`. It records the resolved
version, source, archive hash, and manifest checksums for each installed
bundle—enabling reproducible installs across machines. When a bundle is already
installed but missing from the lock, `sync` repairs the lock entry from the
bundle's on-disk metadata. Commit `sempkg.lock` alongside `sempkg.toml` for
full reproducibility.

### Ad-hoc install (without manifest)

```powershell
# From a registry
sempkg install requests@2.32.3 --registry https://my-registry.example.com

# From a GitHub release URL
sempkg install my-sdk@2.0.0 --url https://github.com/owner/repo/releases/download/v2.0.0/my-sdk-2.0.0.sembundle

# Install globally (~/.sempkg/bundles/)
sempkg install my-sdk@2.0.0 --url <url> --global
```

### Listing installed bundles

```powershell
sempkg list
```

```
Installed bundles:
  aws-sdk   @ 1.11.210  [indexed]  [workspace]  +lance
  requests  @ 2.32.3    [indexed]  [workspace]
  mylib                 [indexed]  [global]     (local pkg)
```

The `+lance` flag indicates the bundle includes a LanceDB documentation index.

### Bundle and package status

```powershell
sempkg status aws-sdk
```

---

## Indexing the Current Workspace

Use `sempkg add .` to treat the current repository like an editable local
dependency. This builds and installs a bundle from the current directory and
stores the source, docs, and exclude settings in `sempkg.toml` so they can be
reused later.

```powershell
# Build from the current workspace and persist the filters
sempkg add . --name mylib --include-source --docs-dir docs --source-dir src --exclude-dir target

# Rebuild the current workspace using the stored settings
sempkg refresh

# Rebuild all manifest dependencies, including local ones
sempkg sync --reinstall
```

`sempkg refresh` only works after the current workspace has been added as a
local dependency with `sempkg add .`.

---

## CodeGraph Queries

All queries are **strictly scoped** to the named package. No cross-package bleed.

```powershell
# Find symbols
sempkg search my-sdk DataFrame
sempkg search my-sdk read_csv -k function
sempkg search my-sdk DataFrame -n 10

# Call graph
sempkg callers my-sdk DataFrame.__init__
sempkg callees my-sdk DataFrame.groupby
sempkg impact  my-sdk DataFrame --depth 5

# Context for a task
sempkg context my-sdk "how to aggregate rows by a column"

# File listing
sempkg files my-sdk
sempkg files my-sdk -f "*.rs"
```

---

## Documentation Search (LanceDB)

Bundles packed with a `--docs-dir` or `--lance-dir` flag contain a LanceDB
full-text index. `sempkg` searches it with BM25 — no model downloads needed.

```powershell
sempkg docs my-sdk "retry policy"
sempkg docs my-sdk "timeout configuration" -n 5
```

### View index metadata

```powershell
sempkg docs-meta my-sdk
```

```
LanceDB metadata for 'my-sdk':
  Table:       docs
  Documents:   148
  Chunks:      612
  FTS enabled: true
  Indexed at:  2026-06-15T10:30:00Z
```

---

## Hybrid Query with Reranking

`sempkg query` is the high-quality search path. It fetches BM25 candidates
from **both** CodeGraph (symbols) and LanceDB (docs), merges the pool, and
scores every candidate against the query using a local
**Qwen3-Reranker-0.6B** cross-encoder running entirely on-device via
llama.cpp. No API calls, no data leaving your machine.

> **Requires:** binary built with `--features reranker` and the model
> downloaded with `sempkg reranker pull`.

### Setup

```powershell
# Build with reranker support
cargo install --path src/sempkg --features reranker

# Download the Qwen3-Reranker-0.6B GGUF (~600 MB, no auth required)
sempkg reranker pull

# Verify it works
sempkg reranker status
sempkg reranker test "how to read a CSV" "read_csv opens a file and returns a DataFrame"
```

### Usage

```powershell
# Hybrid search across both code and docs (default)
sempkg query my-sdk "how to configure retry backoff"

# Docs-only hybrid search
sempkg query my-sdk "retry backoff" --docs

# Code-only hybrid search
sempkg query my-sdk "retry backoff" --code

# Tune result count and candidate pool size
sempkg query my-sdk "retry backoff" -n 10 --top-k 40

# Filter code candidates by symbol kind
sempkg query my-sdk "retry backoff" --code -k function
```

### How it works

| Step | What happens |
|------|-------------|
| 1 | BM25 symbol search across CodeGraph (skipped with `--docs`) |
| 2 | BM25 full-text search across LanceDB (skipped with `--code`) |
| 3 | Candidate pools merged (capped at `top_k`, default 20) |
| 4 | Each `(query, candidate)` pair scored by Qwen3-Reranker in a single forward pass |
| 5 | Candidates re-sorted by score; top `output_n` returned |

When the reranker is unavailable (model missing or not built in), `sempkg query`
falls back to plain BM25 results.

### Compare: search modes

| Command | Backend | Reranker | Best for |
|---------|---------|----------|----------|
| `sempkg search` | CodeGraph BM25 | No | Fast symbol lookup |
| `sempkg docs` | LanceDB BM25 | No | Fast doc search |
| `sempkg query` | Both, merged | Yes | Highest quality, broad queries |

---

## Local Package Management

Local packages are source repositories indexed with CodeGraph directly — no
bundle required. Use `sempkg add .` plus `sempkg refresh` for the editable
workspace flow; use `sempkg pkg` for lower-level CodeGraph-only registration.

```powershell
# Register and index separately
sempkg pkg add mylib C:\Projects\mylib
sempkg pkg add mylib C:\Projects\mylib -d "My internal library"

# List
sempkg pkg list

# Reindex after commits
sempkg pkg reindex mylib

# Build or update the LanceDB docs index
sempkg pkg lance-index mylib
sempkg pkg lance-index mylib --pattern "**/*.md,**/*.rst"

# Status
sempkg pkg status mylib

# Remove (leaves repo and index untouched)
sempkg pkg remove mylib
```

---

## MCP Server

`sempkg mcp` starts the MCP server on stdio. VS Code / GitHub Copilot connect to it and
can call any of the tools listed below.

When a `[reranker]` section is present in `sempkg.toml` and the model is
available, the MCP server loads the reranker at startup and uses it
automatically for relevant tool calls.

### Configuring VS Code

**Workspace-scoped** (`.vscode/mcp.json` — commit this to share with your team):

```json
{
  "servers": {
    "sempkg": {
      "type": "stdio",
      "command": "sempkg",
      "args": ["mcp", "-C", "${workspaceFolder}"]
    }
  }
}
```

**User-scoped** (`%APPDATA%\Code\User\mcp.json` on Windows,
`~/.config/Code/User/mcp.json` on Linux/macOS):

```json
{
  "servers": {
    "sempkg": {
      "type": "stdio",
      "command": "sempkg",
      "args": ["mcp"]
    }
  }
}
```

When started with `-C <workspace>`, `sempkg` uses workspace-local bundles first before
falling back to global bundles. Omit `-C` to use only the global bundle store.

### Available MCP tools

| Tool | Required params | Optional params | Description |
|------|-----------------|-----------------|-------------|
| `list_packages` | — | — | List all local packages and installed bundles with index and docs status |
| `search_symbols` | `package`, `query` | `kind`, `limit` | FTS symbol search via CodeGraph |
| `get_context` | `package`, `task` | — | AI-optimised code context for a natural-language task |
| `get_callers` | `package`, `symbol` | `limit` | Find all callers of a symbol |
| `get_callees` | `package`, `symbol` | `limit` | Find all callees of a symbol |
| `get_impact` | `package`, `symbol` | `depth` | Downstream impact of changing a symbol |
| `list_files` | `package` | `filter` | List source files tracked by CodeGraph |
| `search_docs` | `package`, `query` | `limit` | BM25 full-text search over LanceDB docs index |
| `docs_metadata` | `package` | — | LanceDB index stats: document count, chunk count, FTS status |

All tools accept a `package` name that can be:
- A registered local package name (e.g. `"mylib"`)
- An installed bundle name or `name@version` spec (e.g. `"my-sdk"` or `"my-sdk@1.11.210"`)

---

## Bundle Verification

`sempkg` supports Ed25519 signature verification for bundles downloaded from a registry.

Generate a key pair with `sembundle`:

```powershell
sembundle keygen --output-dir keys/
# Writes: keys/private.pem  keys/public.pem
```

Sign a bundle before publishing:

```powershell
sembundle sign my-sdk-1.11.210.sembundle --key keys/private.pem
```

Verify at install time:

```powershell
sempkg install my-sdk@1.11.210 --registry https://reg.example.com --verify-key keys/public.pem
```

Or add the key to `sempkg.toml` so all `sempkg sync` calls verify automatically:

```toml
[workspace]
verify_key = "keys/public.pem"
```

---

## Full CLI Reference

```
sempkg [OPTIONS] <COMMAND>

Global options:
  -C, --workspace <DIR>    Workspace directory (default: current directory)
                           Env: SEMPKG_WORKSPACE

Workspace / bundle management:
  init [--registry <url>]                     Initialise sempkg.toml
  list                                        List packages and bundles
  add <name>@<ver> [--registry-url <url>]     Add dependency to sempkg.toml
                  [--url <url>]               (direct GitHub release URL)
                  [--registry <name>]
                  [--group <g>]
  remove <name> [--group <g>]                 Remove dependency from sempkg.toml
  sync [--reinstall] [--group <g>]            Install all declared dependencies
       [--all-groups]
  install <name>@<ver> --registry <url>       Install a bundle directly
                       --url <url>            (direct GitHub release URL)
                       [--global]
                       [--verify-key <pem>]
  status <name>                               Show bundle/package status
  repair                                      Recreate missing .codegraph views

Indexing:
  index [<path>] [--name <n>] [--docs-pattern <glob>]
                 [--no-docs] [--no-code] [--global]
                                              Register + index a local repo (idempotent)

CodeGraph queries (scoped to one package):
  search  <pkg> <query> [-k <kind>] [-n <n>]  Symbol search
  callers <pkg> <symbol> [-n <n>]             Find callers
  callees <pkg> <symbol> [-n <n>]             Find callees
  context <pkg> <task>                         AI-optimised context
  impact  <pkg> <symbol> [-d <depth>]          Impact analysis
  files   <pkg> [-f <filter>]                  List files

Documentation search:
  docs      <pkg> <query> [-n <n>]            LanceDB BM25 doc search
  docs-meta <pkg>                             LanceDB index metadata

Hybrid search (requires --features reranker):
  query <pkg> <query> [--docs | --code]       BM25 + Qwen3 reranker
              [-k <kind>] [-n <n>] [--top-k <n>]

MCP server:
  mcp [-C <workspace>]                        Start MCP server (stdio)

Local package management:
  pkg list                                    List local packages
  pkg add    <name> <path> [-d <desc>]        Register local repo
  pkg remove <name>                           Unregister local package
  pkg reindex <name>                          Reindex after commits
  pkg status  <name>                          CodeGraph index status
  pkg lance-index <name> [--pattern <glob>]   Build/update LanceDB doc index

Reranker model management:
  reranker pull   [--gguf-url <url>] [--hf-token <tok>]
                                              Download Qwen3-Reranker GGUF
  reranker status                             Show model path and status
  reranker test   <query> <document>          Score a test (query, doc) pair
```

---

## Workspace Layout

```
<project>/
├── sempkg.toml          Project manifest (dependencies, registries, reranker)
├── sempkg.lock          Locked hashes (auto-generated — commit this)
└── .sempkg/
    └── bundles/
        └── <name>/
            └── <version>/
                ├── manifest.json
                ├── metadata.json
                ├── config.json
                ├── graph/
                ├── embeddings/
                └── lance/          (present only if bundle includes docs)
                    ├── metadata.json
                    └── docs.lance/

~/.sempkg/
├── bundles/             Global bundle store (same layout as above)
├── models/              Reranker GGUF models
│   └── Qwen3-Reranker-0.6B-Q8_0.gguf
└── packages.json        Registered local packages
```

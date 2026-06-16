# sempkg — User Guide

`sempkg` is a native Rust CLI and MCP server for managing
[CGBundle](cgbundle-spec.md) semantic index packages.

It exposes the same intelligence as the Python `codegraph-hub` server but compiles
to a single self-contained binary with no Python runtime dependency.

---

## Contents

1. [Prerequisites](#prerequisites)
2. [Installation](#installation)
3. [Workspace Setup](#workspace-setup)
4. [Installing Bundles](#installing-bundles)
5. [CodeGraph Queries](#codegraph-queries)
6. [Documentation Search](#documentation-search-lancedb)
7. [Local Package Management](#local-package-management)
8. [MCP Server](#mcp-server)
9. [Bundle Verification](#bundle-verification)
10. [Full CLI Reference](#full-cli-reference)

---

## Prerequisites

| Requirement | Notes |
|-------------|-------|
| [CodeGraph](https://github.com/colbymchenry/codegraph) | Must be on `PATH`. Provides the underlying semantic index. Install with `npm install -g @colbymchenry/codegraph` or the platform installer. |
| Rust toolchain | Only required if building from source. Not required if using a pre-built binary. |

---

## Installation

### From source (recommended while in development)

```powershell
# From the repository root
cargo install --path src/sempkg
```

This installs the `sempkg` binary to `~/.cargo/bin/`.

### Build only (no install)

```powershell
cd src/sempkg
cargo build --release
# Binary at: target/release/sempkg (or target/release/sempkg.exe on Windows)
```

---

## Workspace Setup

`sempkg` uses a `sempkg.toml` manifest in the project root to declare bundle dependencies, similar to `Cargo.toml` for Rust crates.

```powershell
cd C:\Projects\my-project
sempkg init
```

This creates a minimal `sempkg.toml`:

```toml
[workspace]
# verify_key = "keys/publisher.pem"   # optional Ed25519 public key for bundle verification

[[registry]]
name = "default"
url  = "https://registry.example.com"

[dependencies]
```

### Adding a registry at init time

```powershell
sempkg init --registry https://my-registry.example.com
```

---

## Installing Bundles

### Via manifest (reproducible)

Add a bundle to `sempkg.toml` and install all dependencies:

```powershell
sempkg add my-sdk@1.2.0 --registry-url https://my-registry.example.com
sempkg sync
```

After the first `sync`, a `sempkg.lock` file is written. Commit it for reproducible installs across machines.

```powershell
sempkg sync --reinstall        # force reinstall even if already present
sempkg sync --group extras     # install an optional dependency group
sempkg sync --all-groups       # install all dependency groups
```

### Ad-hoc install (without manifest)

```powershell
# Workspace-local
sempkg install my-sdk@1.2.0 --registry https://my-registry.example.com

# Global (~/.sempkg/bundles/)
sempkg install my-sdk@1.2.0 --registry https://my-registry.example.com --global
```

### Listing installed bundles

```powershell
sempkg list
```

Output:

```
Installed bundles:
  my-sdk               @ 1.2.0       [indexed]  [workspace]  +lance
  internal-lib         @ 0.9.1       [indexed]  [global]
```

The `+lance` flag indicates the bundle includes a LanceDB documentation index.

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

Bundles packed with `cgbundle build --docs-dir` (or `cgbundle pack --lance-dir`) contain a
LanceDB documentation index. `sempkg` searches it with BM25 full-text search — no external
tools or model downloads required.

### Searching a bundle

```powershell
sempkg docs my-sdk "retry policy"
sempkg docs my-sdk "timeout configuration" -n 5
```

### Viewing index metadata

```powershell
sempkg docs-meta my-sdk
```

Output:

```
LanceDB metadata for 'my-sdk':
  Table:       docs
  Documents:   148
  Chunks:      612
  FTS enabled: true
  Indexed at:  2026-06-15T10:30:00Z
```

### Indexing docs for a local package

If you have a locally registered package (not a bundle), you can build a LanceDB index
over its documentation directory:

```powershell
sempkg pkg lance-index mylib
sempkg pkg lance-index mylib --pattern "**/*.md,**/*.rst"
```

The index is stored at `<package-path>/.sempkg/lance/` and is isolated to that package.
It is then searchable with `sempkg docs mylib <query>`.

---

## Local Package Management

Local packages are source repositories indexed with CodeGraph directly (no bundle required).

```powershell
# Register and index
sempkg pkg add mylib C:\Projects\mylib
sempkg pkg add mylib C:\Projects\mylib -d "My internal library"

# List
sempkg pkg list

# Reindex after commits
sempkg pkg reindex mylib

# Status
sempkg pkg status mylib

# Remove (leaves repo and index untouched)
sempkg pkg remove mylib
```

---

## MCP Server

`sempkg mcp` starts the MCP server on stdio. VS Code / GitHub Copilot connect to it and
can call any of the tools listed below.

### Configuring VS Code

**Workspace-scoped** (`.vscode/mcp.json` — committed to the repo, shared with team):

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

| Tool | Required params | Description |
|------|-----------------|-------------|
| `list_packages` | — | List all local packages and installed bundles with index and docs status |
| `search_symbols` | `package`, `query` | FTS symbol search in a package. Optional: `kind`, `limit` |
| `get_context` | `package`, `task` | AI-optimised code context for a natural-language task description |
| `get_callers` | `package`, `symbol` | Find all callers of a symbol. Optional: `limit` |
| `get_callees` | `package`, `symbol` | Find all callees of a symbol. Optional: `limit` |
| `get_impact` | `package`, `symbol` | Downstream impact of changing a symbol. Optional: `depth` |
| `list_files` | `package` | List source files tracked by CodeGraph. Optional: `filter` |
| `search_docs` | `package`, `query` | BM25 full-text search over LanceDB documentation index. Optional: `limit` |
| `docs_metadata` | `package` | LanceDB index stats: table name, document count, chunk count, FTS status |

All tools accept a `package` name that can be either:
- A registered local package name (e.g. `"mylib"`)
- An installed bundle name or `name@version` spec (e.g. `"my-sdk"` or `"my-sdk@1.2.0"`)

---

## Bundle Verification

`sempkg` supports Ed25519 signature verification for bundles downloaded from a registry.

Generate a key pair with `cgbundle`:

```powershell
cgbundle keygen --output-dir keys/
# Writes: keys/private.pem  keys/public.pem
```

Sign a bundle before publishing:

```powershell
cgbundle sign my-sdk-1.2.0.cgbundle --key keys/private.pem
```

Verify at install time:

```powershell
sempkg install my-sdk@1.2.0 --registry https://reg.example.com --verify-key keys/public.pem
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

Commands:
  init [--registry <url>]                    Initialise sempkg.toml
  list                                       List packages and bundles
  add <name>@<ver> [--registry-url <url>]   Add dependency to sempkg.toml
  remove <name>                              Remove dependency from sempkg.toml
  sync [--reinstall] [--group <g>] [--all-groups]
                                             Install all declared dependencies
  install <name>@<ver> --registry <url>     Install a bundle directly
    [--global] [--verify-key <pem>]
  status <name>                              Show bundle/package status
  repair                                     Recreate missing .codegraph views

  search <pkg> <query> [-k <kind>] [-n <n>] Search symbols (CodeGraph)
  callers <pkg> <symbol> [-n <n>]           Find callers
  callees <pkg> <symbol> [-n <n>]           Find callees
  context <pkg> <task>                       AI-optimised context
  impact  <pkg> <symbol> [-d <depth>]       Impact analysis
  files   <pkg> [-f <filter>]               List files

  docs      <pkg> <query> [-n <n>]          LanceDB documentation search
  docs-meta <pkg>                            LanceDB metadata

  mcp [-C <workspace>]                       Start MCP server (stdio)

  pkg list                                   List local packages
  pkg add <name> <path> [-d <desc>]         Register + index local repo
  pkg remove <name>                          Unregister local package
  pkg reindex <name>                         Reindex local package
  pkg status <name>                          CodeGraph index status
  pkg lance-index <name> [--pattern <glob>]  Build/update LanceDB doc index
```

---

## Workspace Layout

```
<project>/
├── sempkg.toml          Project manifest (dependencies, registries)
├── sempkg.lock          Locked hashes (auto-generated — commit this)
└── .sempkg/
    └── bundles/
        └── <name>/
            └── <version>/   Extracted bundle contents
                ├── manifest.json
                ├── metadata.json
                ├── config.json
                ├── graph/
                ├── embeddings/
                └── lance/       (if bundle has docs index)
                    ├── metadata.json
                    └── docs.lance/

~/.sempkg/
├── bundles/             Global bundle store (same layout as above)
└── packages.json        Registered local packages
```

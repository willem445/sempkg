# sempkg — Developer Guide

`sempkg` is a native Rust CLI and MCP server for managing cgbundle semantic index packages, querying CodeGraph and QMD, and serving context to AI agents.

---

## Architecture

```
src/sempkg/
├── Cargo.toml
└── src/
    ├── main.rs          Entry point, command dispatch
    ├── cli.rs           clap command definitions
    ├── error.rs         Error types (SempkgError)
    ├── manifest.rs      sempkg.toml / sempkg.lock parsing and writing
    ├── store.rs         Bundle store (workspace + global scopes)
    ├── registry.rs      HTTP registry client (index.json, downloads)
    ├── verify.rs        Ed25519 signature verification
    ├── codegraph.rs     codegraph CLI wrapper (scoped queries)
    ├── qmd.rs           QMD SQLite search + CLI wrapper
    ├── packages.rs      Local package registry (~/.sempkg/packages.json)
    └── mcp.rs           MCP JSON-RPC 2.0 server (stdio transport)
```

---

## Building

```sh
cd src/sempkg
cargo build --release
```

The binary is output to `target/release/sempkg`.

Install globally with cargo:
```sh
cargo install --path src/sempkg
```

---

## Workspace Manifest (`sempkg.toml`)

```toml
[workspace]
verify_key = "path/to/pubkey.pem"   # optional Ed25519 PEM public key

[[registry]]
name = "default"
url  = "https://registry.example.com"

[dependencies]
aws-sdk = { version = "1.11.210" }
qt      = { version = "6.7.0", registry = "other-registry" }

[packages]
# Local repos registered for CodeGraph indexing (managed via `sempkg pkg` commands)
mylib = { path = "/home/user/repos/mylib", description = "My library" }
```

The lock file `sempkg.lock` is auto-generated. Commit it for reproducible installs.

---

## CLI Reference

### Workspace management

| Command | Description |
|---------|-------------|
| `sempkg init [--registry <url>]` | Create `sempkg.toml` in current directory |
| `sempkg list` | List all registered packages and installed bundles |
| `sempkg add <name>@<version> [--registry-url <url>]` | Add a dependency to `sempkg.toml` |
| `sempkg remove <name>` | Remove a dependency from `sempkg.toml` |
| `sempkg sync [--reinstall]` | Install all workspace dependencies |
| `sempkg install <name>@<version> --registry <url> [--global] [--verify-key <pem>]` | Install a bundle directly |
| `sempkg status <name>` | Show bundle or package status |

### Local package management

| Command | Description |
|---------|-------------|
| `sempkg pkg list` | List locally registered packages |
| `sempkg pkg add <name> <path> [-d <desc>]` | Register and index a local repo |
| `sempkg pkg remove <name>` | Unregister a local package |
| `sempkg pkg reindex <name>` | Reindex a local package |
| `sempkg pkg status <name>` | Show codegraph index status |

### CodeGraph queries (package-scoped)

All queries operate exclusively on the named package's index — no cross-package bleed.

| Command | Description |
|---------|-------------|
| `sempkg search <pkg> <query> [-k <kind>] [-n <limit>]` | Find symbols by name |
| `sempkg callers <pkg> <symbol> [-n <limit>]` | Find callers of a symbol |
| `sempkg callees <pkg> <symbol> [-n <limit>]` | Find callees of a symbol |
| `sempkg context <pkg> <task>` | Get AI-optimised context for a task |
| `sempkg impact <pkg> <symbol> [-d <depth>]` | Impact analysis |
| `sempkg files <pkg> [-f <filter>]` | List indexed files |

### QMD documentation search

| Command | Description |
|---------|-------------|
| `sempkg docs <bundle> <query> [-n <limit>]` | Search bundle documentation |
| `sempkg docs-meta <bundle>` | Show QMD metadata |

### MCP server

```sh
sempkg mcp [-C /path/to/workspace]
```

Starts the MCP server on stdio. Add to your `.vscode/mcp.json`:

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

---

## MCP Tools

The MCP server exposes these tools to AI agents:

| Tool | Description |
|------|-------------|
| `list_packages` | List all packages and bundles |
| `search_symbols` | Search CodeGraph symbols in a package |
| `get_context` | Get AI context for a task (CodeGraph) |
| `get_callers` | Find callers of a symbol |
| `get_callees` | Find callees of a symbol |
| `get_impact` | Downstream impact analysis |
| `list_files` | List files in a package |
| `search_docs` | BM25/vector search over bundle QMD docs |
| `docs_metadata` | QMD metadata for a bundle |

All tools accept a `package` argument that scopes the query to exactly one package or bundle.

---

## Store Layout

```
# Workspace-local bundles
<workspace>/.sempkg/bundles/<name>/<version>/
    manifest.json
    metadata.json
    config.json
    graph/
    embeddings/
    qmd/           (optional)

# Global bundles
~/.sempkg/bundles/<name>/<version>/

# Local package registry
~/.sempkg/packages.json
```

---

## Bundle Verification

Pass `--verify-key path/to/pubkey.pem` to `sempkg install` or add a `verify_key` to `[workspace]` in `sempkg.toml`. The tool fetches `<bundle>.cgbundle.sig` from the registry and verifies the Ed25519 signature over the bundle's SHA-256 digest.

Generate a key pair with `cgbundle keygen`.

---

## Dependencies

- [CodeGraph](https://github.com/colbymchenry/codegraph) — must be on PATH (`npm install -g @colbymchenry/codegraph`)
- [QMD](https://github.com/tobi/qmd) — optional, required for `docs` commands on local projects (`npm install -g @tobilu/qmd`)

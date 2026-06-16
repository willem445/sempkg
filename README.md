# sempkg

A semantic package manager and MCP server for [CodeGraph](https://github.com/colbymchenry/codegraph) semantic index archives (`.sembundle` files).

`sempkg` installs and queries prebuilt semantic index bundles and exposes them to GitHub Copilot via an MCP server — no Python runtime, no manual context management.

---

## Components

| Component | Language | Description |
|-----------|----------|-------------|
| [`sempkg`](src/sempkg/) | Rust | CLI + MCP server: installs bundles, queries CodeGraph indexes, serves MCP tools |
| [`sembundle`](src/sembundle/) | Rust | CLI: packs, signs, and publishes `.sembundle` archives |
| [`sempkg-registry`](src/sempkg_registry/) | Python | Self-hosted FastAPI server for storing and serving `.sembundle` files |

---

## Quick Start

### Install `sempkg` (from source)

```powershell
cargo install --path src/sempkg
```

### Configure VS Code (workspace)

Add to `.vscode/mcp.json`:

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

### Install a bundle

```powershell
# Initialise a sempkg.toml in your project
sempkg init --registry https://your-registry.example.com

# Add a dependency and install
sempkg add my-sdk@1.2.0
sempkg add pkg@4.6.1 --url https://github.com/org/repo/releases/download/pkg-v4.6.1/pkg-v4.6.1.sembundle
sempkg sync
```

---

## Documentation

- [sempkg User Guide](docs/sempkg.md) — CLI reference, MCP tools, workspace setup
- [SemBundle Format Specification](docs/sembundle-spec.md) — `.sembundle` archive format
- [Registry Server Guide](docs/registry-server.md) — self-hosting the bundle registry
- [ADR-001: LanceDB Documentation Index](docs/adr-001-lancedb-doc-index.md)
- [Vision & Roadmap](docs/vision-roadmap.md)

---

## Prerequisites

| Requirement | Notes |
|-------------|-------|
| [CodeGraph](https://github.com/colbymchenry/codegraph) | Must be on `PATH`. Install with `npm install -g @colbymchenry/codegraph`. |
| Rust toolchain | Required to build `sempkg` and `sembundle` from source. |
| Python 3.11+ | Required only for `sempkg-registry`. |

---

## Development preferences

- Use `uv` for Python dependency management.
- See each component's `DEV-GUIDE.md` for build and test instructions.

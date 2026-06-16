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

Both scripts install `sembundle` and `sempkg` to `~/.local/bin` (Linux/macOS) or `%USERPROFILE%\.local\bin` (Windows). The PowerShell script automatically adds the directory to your user `PATH`.

**Options** (pass after `--` for sh, as flags for ps1):

| Flag | Description |
|------|-------------|
| `--only sembundle` / `-Only sembundle` | Install only `sembundle` |
| `--only sempkg` / `-Only sempkg` | Install only `sempkg` |
| `--version v1.2.0` / `-Version v1.2.0` | Pin a specific release tag |
| `--dir /custom/path` / `-InstallDir C:\path` | Override install directory |

### Build from source

Requires the [Rust toolchain](https://rustup.rs) and a C/C++ compiler (MSVC on Windows, Xcode CLT on macOS, `cmake`+`clang` on Linux).

```sh
cargo install --path src/sembundle
cargo install --path src/sempkg
```

---

## Quick Start

### Configure VS Code (workspace)

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
| Rust toolchain | Required only when building `sempkg` and `sembundle` from source. Not needed when using the install scripts. |
| Python 3.11+ | Required only for `sempkg-registry`. |

---

## Development preferences

- Use `uv` for Python dependency management.
- See each component's `DEV-GUIDE.md` for build and test instructions.

# sempkg

**The missing piece between your AI agent and the code it needs to understand.**

`sempkg` combines the power of [CodeGraph](https://github.com/colbymchenry/codegraph) symbol graphs and [QMD](https://github.com/tobi/qmd)-like documentation indexes into a single Rust binary that doubles as an MCP server — giving GitHub Copilot and other agents instant, structured access to any codebase's semantic intelligence.

![readme-img](README.png)

## Code + docs indexing, scoped to your workspace

Your agent needs two things to reason about a dependency: its **code structure** (symbols, signatures, call graphs) and its **documentation**. Today those live in separate tools, or nowhere at all. `sempkg` indexes both into a single `.sembundle`, scopes it to your workspace, and serves it straight to your agent over MCP.

Most agent tooling instead dumps broad, global indexes into one shared pool — which, over time, pollutes retrieval with symbols and docs from unrelated projects, versions, and stacks. `sempkg` flips that: each project pins and exposes only the bundles it actually depends on. Pull in the context you need daily, leave the rest out.

Version pinning is the icing: because every bundle is tied to the **exact version you ship**, your agent reads the right symbols and the right docs for *your* code — not whatever it scraped off the internet.

Bundles meet you wherever your dependencies live. Eventually you'll be able to pull them from a public registry, but you can already self-host your own registry, or skip a registry entirely and pack and install bundles straight from a GitHub release tag URL or a local folder.

## What you get

- **Symbol search & call graphs** — query function definitions, callers, and callees across indexed codebases without reading source files
- **Semantic doc search** — vector-search over embedded documentation, scoped to the pinned version
- **Version-pinned bundles** — install prebuilt indexes for your exact dependency versions; no drift, no guessing
- **Zero runtime overhead** — single self-contained binary, no Python, no Node, no manual context management
- **Self-hostable registry** — publish and serve your own `.sembundle` archives via `sempkg-registry`

---

## The problem, in depth

Agents are good at using code once the right code is in front of them. The hard
part is getting reliable context for the dependencies your project actually
uses.

In a real workspace, an agent usually has three bad fallback options:

1. Crawl GitHub repos, tags, or random URLs and hope the discovered code matches
   the exact version your project consumes.
2. Read whatever dependency artifacts happen to exist locally in the workspace,
   even though most indexing pipelines intentionally skip installed packages.
3. Fall back to brute-force grep across source trees, vendored files, generated
   code, and partial docs.

Each option breaks in a different way.

If the agent crawls GitHub, it may find the default branch, the wrong release
tag, incomplete source snapshots, or docs that drifted away from the version you
ship. That is enough to produce subtly wrong API calls, outdated signatures, or
references to symbols that never existed in your dependency set.

If the agent tries to rely on dependencies installed into the workspace, it hits
another problem: most code indexing tools do not index those dependencies at
all. That is often the correct tradeoff. Dependency directories are full of
noise: generated files, build outputs, transitive packages, duplicated vendored
code, irrelevant symbols, and giant surfaces that would pollute retrieval for
the project the agent is actually trying to change. So the dependency code is
present on disk, but absent from the agent's structured search tools.

At that point, the agent often degrades to inefficient grepping. It can scan raw
files, but it loses the higher-level structure it actually needs: symbol
definitions, call relationships, package boundaries, version identity, and the
ability to distinguish the one relevant API from thousands of adjacent lines of
junk.

Documentation-focused tools only solve part of this. Some systems index docs,
and some build a bridge from a code graph to a documentation index. That helps
retrieval, but it still fails if the docs are not pinned to the exact version in
your build, or if the docs simply do not describe the real implementation well
enough. Agents do not just need prose about a library; they need the actual code
surface they are calling: real symbols, real signatures, real relationships, and
real version boundaries.

That is the gap `sempkg` is built to close. Instead of making agents choose
between wrong-version GitHub crawling, unindexed local dependencies, or
documentation that may not reflect reality, `sempkg` installs version-pinned
semantic bundles directly into the workspace and exposes them as clean,
structured context.

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
curl -fsSL https://raw.githubusercontent.com/willem445/sempkg/main/install.sh | sh
```

**Windows (PowerShell):**
```powershell
irm https://raw.githubusercontent.com/willem445/sempkg/main/install.ps1 | iex
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

# Add & index a dependency directly from Github (bypass index)
sempkg add https://github.com/pandas-dev/pandas/releases/tag/v3.0.3 --full
```

After `sempkg sync`, the installed bundles live in your workspace alongside the
rest of your project state:

```text
my-workspace/
├── .vscode/
│   └── mcp.json
├── src/
├── sempkg.toml
├── sempkg.lock
└── .sempkg/
  └── bundles/
    └── my-sdk/
      └── 1.2.0/
        ├── manifest.json
        ├── metadata.json
        ├── config.json
        ├── graph/
        ├── embeddings/
        └── lance/
          ├── metadata.json
          └── docs.lance/
```

That keeps semantic indexes scoped to the current repository, just like other
workspace-local tooling and dependency metadata.

### GitHub authentication (private / enterprise)

When using private repositories or restricted GitHub hosts (GitHub Enterprise),
set a token environment variable before running `sempkg add`.

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
sempkg add https://github.company.com/org/repo/releases/tag/v3.0.3 --full
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

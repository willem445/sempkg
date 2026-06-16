# codegraph-hub

A multi-repo [codegraph](https://github.com/colbymchenry/codegraph) MCP server for **GitHub Copilot**.

Registers locally cloned internal codebases, indexes them with codegraph, and exposes a single MCP endpoint that Copilot uses to query symbols, call graphs, and source files — without needing to drag reference files into the context window each session.

## Why this exists

When you are working in one repository, you often still need context from other codebases:

- Internal Python packages used by your app
- A locally installed SDK
- Internal frameworks shared across repos
- Public dependencies cloned locally when you want exact source/API behavior

The usual workaround is to manually paste external code into chat or attach files repeatedly.
That increases token usage, creates noisy context windows, and does not scale well across sessions.

[codegraph](https://github.com/colbymchenry/codegraph) is excellent at indexing and querying code efficiently, but it is typically centered on the currently open codebase.

`codegraph-hub` provides a wrapper MCP server in front of codegraph so you can register and index multiple local folders, then expose them through one Copilot-accessible endpoint.

This gives Copilot direct, queryable access to symbols, call graphs, and source across your local dependency graph without manual copy/paste context management.

---

## Prerequisites

- Python 3.11+
- [codegraph](https://github.com/colbymchenry/codegraph) on your PATH

  ```powershell
  irm https://raw.githubusercontent.com/colbymchenry/codegraph/main/install.ps1 | iex
  ```

---

## Installation

```powershell
cd C:\Projects\python\codegraph-hub
pip install -e .
```

Or with [uv](https://docs.astral.sh/uv/):

```powershell
uv pip install -e .
```

To run the bundle registry server locally:

```powershell
uv pip install -e .[registry]
```

---

## Configure GitHub Copilot (VS Code)

Add the server to your **user-level** MCP config so it is available in every project.

**`%APPDATA%\Code\User\mcp.json`** (or via VS Code: *Copilot → MCP Servers → Add Server*):

```json
{
  "servers": {
    "codegraph-hub": {
       "type": "stdio",
       "command": "codegraph-hub",
       "args": ["serve"]
     }
  }
}
```

> If you installed with `pip install -e .`, the `codegraph-hub` script is also on your PATH:
> ```json
> { "command": "codegraph-hub", "args": [] }
> ```

Restart VS Code / reload the Copilot extension after saving the config.

---

## Registering internal packages

Once the MCP server is running you can register packages **directly through Copilot chat**:

Copilot will call `add_package("pandas", "C:\\Projects\\internal\\pandas", ...)` which:

1. Adds the entry to `~/.codegraph-hub/packages.json`
2. Runs `codegraph init --index` on the repo
3. Confirms when complete

Or call it yourself via any terminal using the CLI interface.

Register the pandas package cloned to C:\projects\pandas:

```sh
codegraph-hub add pandas C:\projects\pandas -d "Data analysis manipulation library"
```

---

## Available MCP Tools

| Tool | Description |
|------|-------------|
| `list_packages` | Show all registered packages and their index status |
| `add_package(name, path, description?)` | Register + index a local repo |
| `remove_package(name)` | Remove from registry (leaves repo untouched) |
| `reindex_package(name)` | Sync index after new commits |
| `package_status(name)` | Index statistics (symbol count, last sync) |
| `search_package(name, query, kind?)` | FTS symbol search |
| `get_context(name, task)` | AI-optimized context for a task description |
| `get_callers(name, symbol)` | What calls this function |
| `get_callees(name, symbol)` | What this function calls |
| `get_impact(name, symbol, depth?)` | Blast radius of changing a symbol |
| `list_package_files(name, filter?)` | File tree of the package |
| `read_file(name, file_path)` | Full source of a specific file |

---

## Example Copilot workflow

```sh
# In any project chat:
User: How do I aggregate sales by region with pandas.groupby?

Copilot: [calls list_packages → sees pandas is registered]
         [calls get_context("pandas", "DataFrame.groupby usage and aggregation examples")]
         → returns relevant symbols, entry points, code snippets

Copilot: Here's a typical pattern:
     df.groupby("region")["sales"].agg(["sum", "mean", "count"])
```

---

## Package registry location

`~/.codegraph-hub/packages.json` — a plain JSON file you can inspect or edit manually.

```json
{
  "pandas": {
    "name": "pandas",
    "path": "C:\\Projects\\internal\\pandas",
    "description": "Dataframe analysis tool"
  }
}
```

---

## Keeping indexes fresh

After pulling new commits to an internal package, run a re-index:

```
Reindex the pandas package
```

Or from a terminal:

```powershell
codegraph sync C:\projects\pandas
```

---

## CLI Commands

All functionality is available via command-line. Get help with `codegraph-hub --help` or `codegraph-hub <command> --help`.

Bundle registry and distribution docs:
- [docs/registry-server.md](docs/registry-server.md)

### Package Management

| Command | Description | Example |
|---------|-------------|---------|
| `list` | Show all registered packages | `codegraph-hub list` |
| `add <name> <path>` | Register and index a local repo | `codegraph-hub add pandas C:\projects\pandas` |
| `add <name> <path> -d <desc>` | Register with a description | `codegraph-hub add pandas C:\projects\pandas -d "Data analysis library"` |
| `remove <name>` | Remove from registry (leaves repo untouched) | `codegraph-hub remove pandas` |
| `reindex <name>` | Sync index after new commits | `codegraph-hub reindex pandas` |
| `status <name>` | Show index statistics (symbol count, last sync) | `codegraph-hub status pandas` |

### Symbol Search & Analysis

| Command | Description | Example |
|---------|-------------|---------|
| `search [package] <query>` | Full-text search for symbols | `codegraph-hub search pandas read_csv` |
| `search <query>` | Search across all packages | `codegraph-hub search assert` |
| `search [package] <query> -k <kind>` | Filter by kind (function, class, method, etc.) | `codegraph-hub search pandas read_csv -k function` |
| `search [package] <query> -n <limit>` | Limit results (default: 20) | `codegraph-hub search pandas DataFrame -n 10` |
| `symbol [package] <symbol>` | Show source code of a symbol | `codegraph-hub symbol pandas DataFrame` |
| `symbol [package] <symbol> -k <kind>` | Disambiguate if multiple definitions exist | `codegraph-hub symbol pandas DataFrame -k class` |
| `symbol [package] <symbol> -c <lines>` | Include surrounding context lines | `codegraph-hub symbol pandas read_csv -c 5` |
| `symbol <symbol>` | Search for symbol across all packages (stops at first match) | `codegraph-hub symbol read_csv` |

### Call Graph Analysis

| Command | Description | Example |
|---------|-------------|---------|
| `callers [package] <symbol>` | Find what calls this function/method | `codegraph-hub callers pandas read_csv` |
| `callers <symbol>` | Search across all packages | `codegraph-hub callers merge` |
| `callees [package] <symbol>` | Find what this function/method calls | `codegraph-hub callees pandas merge` |
| `callees <symbol>` | Search across all packages | `codegraph-hub callees groupby` |
| `impact [package] <symbol>` | Analyze blast radius of changing a symbol | `codegraph-hub impact pandas DataFrame` |
| `impact [package] <symbol> --depth <n>` | Trace depth (default: 3) | `codegraph-hub impact pandas DataFrame --depth 5` |

### File & Context Operations

| Command | Description | Example |
|---------|-------------|---------|
| `files [package]` | List file structure of a package | `codegraph-hub files pandas` |
| `files [package] <filter>` | Filter by glob pattern | `codegraph-hub files pandas "*.py"` |
| `read <package> <file>` | Print entire file | `codegraph-hub read pandas pandas/core/frame.py` |
| `read <package> <file> <start>` | Print from line (1-indexed) | `codegraph-hub read pandas pandas/core/frame.py 10` |
| `read <package> <file> <start> <end>` | Print line range (inclusive) | `codegraph-hub read pandas pandas/core/frame.py 10 25` |
| `context [package] <task>` | Get AI-optimized context for a task description | `codegraph-hub context pandas how to use DataFrame.groupby and agg` |

### Server

| Command | Description | Example |
|---------|-------------|---------|
| `serve` | Start the MCP server (used by VS Code / Copilot) | `codegraph-hub serve` |

### Bundle Registry Commands

| Command | Description | Example |
|---------|-------------|---------|
| `bundle add <pkg>@<ver> --registry <name>` | Add dependency to manifest, install, update lock | `codegraph-hub bundle add my-lib@1.2.0 --registry default` |
| `bundle add <pkg>@<ver> --registry-url <url>` | Same but with inline URL | `codegraph-hub bundle add my-lib@1.2.0 --registry-url http://127.0.0.1:8765` |
| `bundle sync` | Install all deps from `codegraph-hub.toml` (reproducible) | `codegraph-hub bundle sync` |
| `bundle sync --verify-key <path>` | Sync with Ed25519 signature verification | `codegraph-hub bundle sync --verify-key keys/publisher.pem` |
| `bundle sync --reinstall` | Force reinstall even if already present | `codegraph-hub bundle sync --reinstall` |
| `bundle lock` | Refresh `codegraph-hub.lock` hashes without installing | `codegraph-hub bundle lock` |
| `bundle search-registry <url>` | Show packages and versions available on a registry server | `codegraph-hub bundle search-registry http://127.0.0.1:8765` |
| `bundle install <pkg>@<ver> --registry <url>` | Ad-hoc install without manifest | `codegraph-hub bundle install my-lib@1.2.0 --registry http://127.0.0.1:8765` |
| `bundle install <pkg>@<ver> --registry <url> --global` | Install into global scope | `codegraph-hub bundle install my-lib@1.2.0 --registry http://127.0.0.1:8765 --global` |
| `bundle list` | List workspace and global installed bundles | `codegraph-hub bundle list` |
| `bundle list --workspace` | List workspace-only bundles | `codegraph-hub bundle list --workspace` |
| `bundle list --global` | List global-only bundles | `codegraph-hub bundle list --global` |
| `bundle remove <pkg>@<ver>` | Remove bundle from workspace scope | `codegraph-hub bundle remove my-lib@1.2.0` |
| `bundle remove <pkg>@<ver> --global` | Remove bundle from global scope | `codegraph-hub bundle remove my-lib@1.2.0 --global` |

### cgbundle Registry Publishing

| Command | Description | Example |
|---------|-------------|---------|
| `publish <bundle_path> --registry <url> --token <token>` | Publish a bundle archive to a registry | `cgbundle publish .\my-lib-1.2.0.cgbundle --registry http://127.0.0.1:8765 --token <TOKEN>` |

`cgbundle publish` also supports environment variables:
- `CGBUNDLE_REGISTRY_URL`
- `CGBUNDLE_TOKEN`

See [docs/registry-server.md](docs/registry-server.md) for self-hosting, token management, and full publish/pull workflows.

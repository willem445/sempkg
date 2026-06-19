# ✅ **MASTER TODO LIST FOR CODEGRAPH‑HUB ECOSYSTEM**

Below are **15 sequential prompts**, each representing one major deliverable in the architecture.

---

# **TASK 1 — Define the SemBundle Specification**
**Prompt to give your agent:**

```
Create a formal specification for a "SemBundle" file format used to package CodeGraph (https://github.com/colbymchenry/codegraph) index artifacts.

Requirements:
- The bundle must contain: graph/, embeddings/, metadata.json, config.json.
- The bundle must be immutable and versioned.
- The bundle must include a manifest.json with:
  - name
  - version
  - source_repo
  - commit_hash or tag
  - created_at timestamp
  - codegraph version used
  - SHA256 checksums for all files
- The bundle must be packaged as a .tar.gz file.
- The spec must include directory layout, required fields, and validation rules.

Output:
- A Markdown document describing the full spec.
```

---

# **TASK 2 — Implement a SemBundle Packer CLI**
**Prompt:**

```
Implement a CLI tool called `SemBundle pack`.

Requirements:
- Input: path to a CodeGraph output directory.
- Output: a .SemBundle tar.gz file.
- Generate manifest.json automatically.
- Compute SHA256 checksums for all files.
- Validate required directories exist.
- Write clean error messages.
- Use Rust or Go (your choice).
- Provide unit tests for:
  - missing directories
  - checksum generation
  - manifest correctness
```

---

# **TASK 3 — Implement a SemBundle Unpacker**
**Prompt:**

```
Implement an unpacking feature into sempkg CLI tool. This will eventually be connected up to a "sempkg add dep@x.y.z" command which pulls down the bundle from the register, unpacks it (this feature) into a repository equivalent of a python .venv or npm node_modules. This task is just implementing the unpack feature + unit tests.

Requirements:
- Input: .SemBundle file
- Output: extracted directory
- Validate checksums
- Validate manifest.json
- Provide a --target-dir option
- Provide unit tests for:
  - corrupted bundle
  - checksum mismatch
  - missing manifest
```

---

# **TASK 4 — Build the Local Registry Cache**
**Prompt:**

```
Implement a local registry cache for sempkg.

TBDL should support repository specific node_modules style or globally in the ~/.sempkg user folder.

Requirements:
- Store registry.json in ~/.sempkg/registry.json
- Store bundles in ~/.sempkg/bundles/<package>/<version>/
- Implement:
  - load_registry()
  - save_registry()
  - list_packages()
  - list_versions(package)
  - resolve(package, version)
- Registry format:
  {
    "packages": {
      "qt": ["6.7.0"],
      "aws-sdk": ["1.11.210"]
    }
  }
```

---

# **TASK 5 — Implement Remote Registry Sync**
**Prompt:**

```
Implement a registry sync system.

Requirements:
- Fetch index.json from a remote URL.
- Merge remote registry with local registry.
- Support multiple registries.
- Detect new versions.
- Update local registry.json.
- Provide a CLI command: `sempkg registry sync`.
```

---

# **TASK 6 — Implement Bundle Downloader**
**Prompt:**

```
Implement a bundle downloader.

Requirements:
- Download .SemBundle files from registry URLs.
- Verify SHA256 checksum from manifest.json.
- Extract into ~/.sempkg/bundles/<package>/<version>/
- Cache bundles locally.
- Provide CLI: `sempkg bundle install <package>@<version>`
```

---

# **TASK 7 — Implement Project Profiles (Version Pinning)**
**Prompt:**

```
Implement project profiles for version pinning.

Requirements:
- File: project.codegraph.json in project root.
- Format:
  {
    "dependencies": {
      "aws-sdk": "1.11.210",
      "qt": "6.7.0"
    }
  }
- Implement:
  - load_project_profile()
  - resolve_dependencies()
  - auto-install missing bundles
- Provide CLI: `sempkg project sync`
```

---

# **TASK 8 — Implement Dynamic Graph Loader**
**Prompt:**

```
Implement a dynamic graph loader for sempkg.

Requirements:
- Load CodeGraph graphs from bundle directories.
- Unload graphs to free memory.
- Maintain a map: { (package, version) → GraphInstance }
- Provide:
  - load_graph(package, version)
  - unload_graph(package, version)
  - list_loaded_graphs()
- Ensure thread safety.
```

---

# **TASK 9 — Implement Multi-Graph MCP Server**
**Prompt:**

```
Extend the MCP server to support multiple graphs.

Requirements:
- Add tools:
  - list_graphs
  - select_graph
  - search_symbols
  - semantic_search
  - list_callers
  - list_callees
- The server must route queries to the active graph.
- Support switching graphs at runtime.
- Support multiple active graphs simultaneously.
```

---

# **TASK 10 — Implement Cross-Graph Query Engine**
**Prompt:**

```
Implement cross-graph semantic search.

Requirements:
- Allow searching across multiple loaded graphs.
- Merge results with ranking.
- Provide tool: search_across(graphs[], query)
- Provide tool: explain_symbol(symbol_id)
- Ensure symbol IDs include package/version namespace.
```

---

# **TASK 11 — Build the Index Builder GitHub Action**
**Prompt:**

```
Create a GitHub Action workflow that:

1. Detects new tags.
2. Clones the repo.
3. Runs CodeGraph indexing.
4. Packages the result into a .SemBundle.
5. Uploads the bundle to the registry.
6. Updates index.json.

Requirements:
- Use matrix builds for multiple languages.
- Provide caching.
- Provide error reporting.
```

---

# **TASK 12 — Implement Registry Generator**
**Prompt:**

```
Implement a registry generator tool.

Requirements:
- Scan a directory of bundles.
- Generate index.json.
- Generate per-version manifest.json.
- Validate bundle structure.
- Provide CLI: `registry build <path>`
```

---

# **TASK 13 — Implement a Static Registry Server**

## ✅ Implemented

### Registry server — `SemBundle-registry` (`src/SemBundle_registry/`)
- FastAPI application (`app.py`) with endpoints:
  - `GET  /bundles` — list all published bundles
  - `GET  /bundles/{name}` — list versions for a package
  - `GET  /bundles/{name}/{version}/{filename}` — download a bundle file
  - `POST /bundles/{name}/{version}` — upload/publish a bundle (token-authenticated)
  - `DELETE /bundles/{name}/{version}` — remove a bundle (token-authenticated)
- Token-based authentication via `Authorization: Bearer <token>` (`auth.py`)
- File-backed storage with manifest index (`storage.py`)
- `Dockerfile` for self-hosted deployment

### `SemBundle publish` — Rust CLI command (`src/SemBundle/`)
- `SemBundle publish <bundle.SemBundle> --registry <url> --token <token>`
- Streams the bundle to the registry's upload endpoint
- Validates server response and reports success/failure

### Bundle store — `sempkg` (`src/sempkg/bundle_store.py`)
- `BundleStore` class managing workspace-scoped and global bundle installations
- Workspace store: `.sempkg/bundles/` relative to the active workspace
- Global store: `~/.sempkg/bundles/` shared across all workspaces
- `install(bundle_path)` — validate checksums, extract, write manifest
- `install_from_registry(package, version, registry_url)` — download then install
- `list_installed()` — enumerate all installed bundles
- `resolve(name, version)` — return extracted bundle dir path or `None`
- `remove(name, version)` — uninstall a bundle

### CLI commands (`src/sempkg/cli.py`)
- `bundle install <name>@<version> [--registry <url>]`
- `bundle list [--workspace <dir>]`
- `bundle remove <name>@<version> [--workspace <dir>]`
- `bundle search-registry <query> [--registry <url>]`

### MCP tools (`src/sempkg/server.py`)
- `list_bundle_packages(workspace_dir)` — list workspace and global installed bundles
- `get_bundle_info(name, version, workspace_dir)` — manifest details for a bundle
- `search_bundle_symbol(bundle_name, bundle_version, query, workspace_dir)` — query symbols in a bundle's pre-built CodeGraph index
- `list_bundle_callers(bundle_name, bundle_version, symbol, workspace_dir)` — callers graph from bundle
- `list_bundle_callees(bundle_name, bundle_version, symbol, workspace_dir)` — callees graph from bundle

---

# **TASK 14 — Implement sempkg CLI UX**
**Prompt:**

```
Implement the main CLI commands:

- sempkg registry sync
- sempkg bundle install <pkg>@<ver>
- sempkg project sync
- sempkg list
- sempkg mcp serve

Requirements:
- Clean UX
- Helpful error messages
- Colorized output
- Progress bars for downloads
```

---

# **TASK 15 — Write Documentation**
**Prompt:**

```
Write documentation for:

- SemBundle format
- Registry architecture
- How to publish bundles
- How to consume bundles
- How to configure project profiles
- How to run the MCP server

Output:
- Markdown files suitable for GitHub.
```

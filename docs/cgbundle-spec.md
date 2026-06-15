# CGBundle Format Specification

**Version:** 1.1.0
**Status:** Draft

---

## 1. Overview

A **CGBundle** (`.cgbundle`) is a portable, immutable, versioned archive that packages a prebuilt [CodeGraph](https://github.com/colbymchenry/codegraph) semantic index for a specific revision of a source repository.

CGBundles allow codebases to publish semantic intelligence artifacts alongside their releases. Consumers — agents, IDEs, and tools — can download and mount a bundle without ever cloning the source repository.

---

## 2. File Naming Convention

```
<name>-<version>.cgbundle
```

| Component | Description |
|-----------|-------------|
| `name`    | Package/library name (lowercase, hyphens allowed, no spaces) |
| `version` | Semantic version (`MAJOR.MINOR.PATCH`) or tag identifier |

**Examples:**

```
aws-sdk-1.11.210.cgbundle
qt-6.7.0.cgbundle
ros2-humble.cgbundle
```

A `.cgbundle` file is a **gzip-compressed tar archive** (`.tar.gz` renamed to `.cgbundle`). All paths inside the archive are relative and rooted at a single top-level directory matching the bundle name:

```
<name>-<version>/
```

---

## 3. Directory Layout

```
<name>-<version>/
├── manifest.json       # Bundle metadata and checksums (required)
├── metadata.json       # Source repo and indexing metadata (required)
├── config.json         # CodeGraph configuration used during indexing (required)
├── graph/              # CodeGraph graph store directory (required)
│   └── ...             # Internal graph files (implementation-defined)
├── embeddings/         # CodeGraph semantic embedding vectors (required)
│   └── ...             # Internal embedding files (implementation-defined)
└── qmd/                # QMD documentation index (optional)
    ├── index/          # Project-local QMD SQLite database
    ├── embeddings/     # Format-neutral vector export
    ├── metadata.json   # QMD indexing metadata
    ├── model.gguf      # GGUF embedding model (optional)
    └── config.json     # QMD collection configuration
```

The top-level entries `manifest.json`, `metadata.json`, `config.json`, `graph/`, and `embeddings/` are **required**. The `qmd/` entry is **optional**. A bundle missing any required entry is invalid.

---

## 4. `manifest.json` — Bundle Manifest

The manifest is the authoritative descriptor for a bundle. It is generated at pack time and must not be modified after creation.

### 4.1 Schema

```json
{
  "spec_version":      "<string>",
  "name":              "<string>",
  "version":           "<string>",
  "source_repo":       "<string>",
  "commit_hash":       "<string>",
  "tag":               "<string | null>",
  "created_at":        "<ISO 8601 UTC datetime>",
  "codegraph_version": "<string>",
  "extensions":        ["<string>", ...],
  "checksums": {
    "<relative-file-path>": "<sha256-hex>",
    ...
  }
}
```

### 4.2 Field Definitions

| Field               | Type            | Required | Description |
|---------------------|-----------------|----------|-------------|
| `spec_version`      | string          | Yes      | CGBundle spec version this bundle conforms to (e.g. `"1.0.0"`) |
| `name`              | string          | Yes      | Package name. Must match the filename prefix. Pattern: `^[a-z0-9][a-z0-9\-]*[a-z0-9]$` |
| `version`           | string          | Yes      | Package version. Must match the filename version component. |
| `source_repo`       | string          | Yes      | Canonical URL of the source repository (e.g. `"https://github.com/org/repo"`) |
| `commit_hash`       | string          | Yes      | Full 40-character Git SHA of the indexed commit |
| `tag`               | string or null  | Yes      | Git tag associated with this version, or `null` if none |
| `created_at`        | string          | Yes      | ISO 8601 UTC timestamp of when the bundle was created (e.g. `"2025-08-01T14:30:00Z"`) |
| `codegraph_version` | string          | Yes      | Version of CodeGraph used to produce the index |
| `extensions`        | array of string | No       | Optional bundle extensions present. Must include `"qmd"` when `qmd/` is present. Omit or use `[]` when no extensions are bundled. |
| `checksums`         | object          | Yes      | Map of relative file paths (within the bundle) to their SHA-256 hex digests. Must cover every file except `manifest.json` itself. |

### 4.3 Checksums Coverage

The `checksums` map must include an entry for **every file** in the bundle **except** `manifest.json`. Directories are not listed. Relative paths use forward slashes regardless of OS.

**Example:**

```json
{
  "spec_version": "1.1.0",
  "name": "aws-sdk",
  "version": "1.11.210",
  "source_repo": "https://github.com/aws/aws-sdk-cpp",
  "commit_hash": "d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3",
  "tag": "1.11.210",
  "created_at": "2025-08-01T14:30:00Z",
  "codegraph_version": "0.3.1",
  "extensions": [],
  "checksums": {
    "metadata.json": "a1b2c3d4e5f6...",
    "config.json": "b2c3d4e5f6a1...",
    "graph/nodes.bin": "c3d4e5f6a1b2...",
    "graph/edges.bin": "d4e5f6a1b2c3...",
    "embeddings/vectors.bin": "e5f6a1b2c3d4..."
  }
}
```

---

## 5. `metadata.json` — Source Metadata

Describes the source repository and the indexing context. This file is generated by the packer from CodeGraph's output and source control information.

### 5.1 Schema

```json
{
  "name":          "<string>",
  "version":       "<string>",
  "source_repo":   "<string>",
  "commit_hash":   "<string>",
  "tag":           "<string | null>",
  "language":      "<string>",
  "indexed_paths": ["<string>", ...],
  "created_at":    "<ISO 8601 UTC datetime>"
}
```

### 5.2 Field Definitions

| Field           | Type            | Required | Description |
|-----------------|-----------------|----------|-------------|
| `name`          | string          | Yes      | Package name (must match `manifest.json`) |
| `version`       | string          | Yes      | Package version (must match `manifest.json`) |
| `source_repo`   | string          | Yes      | Source repository URL (must match `manifest.json`) |
| `commit_hash`   | string          | Yes      | Git SHA (must match `manifest.json`) |
| `tag`           | string or null  | Yes      | Git tag (must match `manifest.json`) |
| `language`      | string          | Yes      | Primary language indexed (e.g. `"python"`, `"cpp"`, `"rust"`) |
| `indexed_paths` | array of string | Yes      | List of paths within the repo that were indexed (relative to repo root) |
| `created_at`    | string          | Yes      | ISO 8601 UTC timestamp (must match `manifest.json`) |

---

## 6. `config.json` — CodeGraph Configuration

A verbatim copy of the CodeGraph configuration file used to produce the index. Its exact schema is defined by the CodeGraph tool. It is included in the bundle to allow reproducible re-indexing and to communicate indexing parameters to consumers.

At minimum, `config.json` must be a valid JSON object (`{}`).

---

## 7. `graph/` — Graph Store

The `graph/` directory contains the CodeGraph graph store artifacts produced by the indexer. The internal structure is **implementation-defined by CodeGraph** and may vary across CodeGraph versions.

Consumers must treat `graph/` as opaque and load it via the CodeGraph API. At least one file must be present inside `graph/`; an empty directory is invalid.

---

## 8. `embeddings/` — Embedding Vectors

The `embeddings/` directory contains precomputed semantic embedding vectors for symbols in the graph. The internal format is **implementation-defined by CodeGraph**.

At least one file must be present inside `embeddings/`; an empty directory is invalid.

---

## 9. `qmd/` — Documentation Index

The `qmd/` directory is an **optional** extension that bundles a per-repository documentation index produced by [QMD](https://github.com/tobi/qmd) (Query Markup Documents). QMD provides hybrid BM25 full-text search, vector semantic search, and LLM re-ranking — all running locally via GGUF models.

When present, `qmd/` allows consumers (agents, IDEs, MCP clients) to query the repository's documentation offline without cloning the source repository or relying on a shared global QMD instance. The index is scoped exclusively to the repository being bundled.

When `qmd/` is present, `manifest.json` must include `"qmd"` in its `extensions` array.

### 9.1 Sub-directory Layout

```
qmd/
├── index/             # Project-local QMD SQLite database (required)
│   └── index.sqlite   # Documents, FTS5 index, and sqlite-vec embeddings
├── embeddings/        # Format-neutral vector export (required)
│   └── ...            # Implementation-defined flat binary or JSONL chunks
├── metadata.json      # QMD indexing metadata (required)
├── model.gguf         # GGUF embedding model used during indexing (optional)
└── config.json        # QMD collection configuration (required)
```

All entries except `model.gguf` are **required** when `qmd/` is present. When `model.gguf` is omitted, the `embed_model` field in `qmd/metadata.json` records the HuggingFace URI so consumers can download it on demand.

### 9.2 `qmd/index/` — QMD Database

Contains the project-local QMD SQLite database (`index.sqlite`). This database is produced using a scoped, project-local QMD invocation (see §9.7) and holds a single collection representing the repository being bundled. It includes:

- Document content and metadata (`documents` table)
- FTS5 full-text index (`documents_fts`)
- Vector embedding chunks (`content_vectors`, `vectors_vec` via sqlite-vec)
- Collection and context configuration (`collections`, `path_contexts`)
- LLM response cache (`llm_cache`)

Consumers with QMD installed can mount and query this database directly:

```js
import { createStore } from '@tobilu/qmd'

const store = await createStore({ dbPath: './qmd/index/index.sqlite' })
const results = await store.search({ query: "authentication flow" })
await store.close()
```

### 9.3 `qmd/embeddings/` — Vector Export

A format-neutral export of the embedding vectors already stored in `qmd/index/index.sqlite`. This allows lightweight consumers that do not have QMD or the sqlite-vec extension installed to use the precomputed embeddings directly.

The internal format is **implementation-defined**. The `embeddings_format` field in `qmd/metadata.json` must identify the format (e.g. `"binary-f32"`, `"jsonl"`). At least one file must be present; an empty `qmd/embeddings/` directory is invalid.

### 9.4 `qmd/metadata.json` — QMD Metadata

#### Schema

```json
{
  "qmd_version":       "<string>",
  "embed_model":       "<string>",
  "embed_model_hash":  "<string | null>",
  "chunk_strategy":    "<string>",
  "embeddings_format": "<string>",
  "embedding_dim":     "<number>",
  "collection_name":   "<string>",
  "document_count":    "<number>",
  "chunk_count":       "<number>",
  "indexed_paths":     ["<string>", ...],
  "created_at":        "<ISO 8601 UTC datetime>"
}
```

#### Field Definitions

| Field               | Type            | Required | Description |
|---------------------|-----------------|----------|-------------|
| `qmd_version`       | string          | Yes      | Version of QMD used to produce the index (e.g. `"2.5.3"`) |
| `embed_model`       | string          | Yes      | HuggingFace URI of the embedding model used (e.g. `"hf:ggml-org/embeddinggemma-300M-GGUF/embeddinggemma-300M-Q8_0.gguf"`) |
| `embed_model_hash`  | string or null  | Yes      | SHA-256 hex digest of `qmd/model.gguf`, or `null` if `model.gguf` was omitted |
| `chunk_strategy`    | string          | Yes      | Chunking strategy: `"regex"` (default) or `"auto"` (AST-aware, recommended for source code) |
| `embeddings_format` | string          | Yes      | Format of files in `qmd/embeddings/` (e.g. `"binary-f32"`, `"jsonl"`) |
| `embedding_dim`     | number          | Yes      | Dimensionality of each embedding vector |
| `collection_name`   | string          | Yes      | QMD collection name used during indexing (should match bundle `name`) |
| `document_count`    | number          | Yes      | Total number of documents indexed |
| `chunk_count`       | number          | Yes      | Total number of embedding chunks generated |
| `indexed_paths`     | array of string | Yes      | Glob patterns or paths indexed (relative to repo root) |
| `created_at`        | string          | Yes      | ISO 8601 UTC timestamp (must match `manifest.json` `created_at`) |

#### Example

```json
{
  "qmd_version": "2.5.3",
  "embed_model": "hf:ggml-org/embeddinggemma-300M-GGUF/embeddinggemma-300M-Q8_0.gguf",
  "embed_model_hash": "a1b2c3d4e5f6...",
  "chunk_strategy": "auto",
  "embeddings_format": "binary-f32",
  "embedding_dim": 768,
  "collection_name": "aws-sdk",
  "document_count": 1842,
  "chunk_count": 9231,
  "indexed_paths": ["docs/**/*.md", "**/*.rst", "README.md"],
  "created_at": "2025-08-01T14:30:00Z"
}
```

### 9.5 `qmd/model.gguf` — Embedding Model

The GGUF embedding model used to generate vectors in `qmd/index/index.sqlite` and `qmd/embeddings/`. Including the model enables fully offline operation: a consumer can load the model and perform semantic queries against the index without downloading anything.

This file is **optional**. When omitted, the `embed_model` field in `qmd/metadata.json` provides the HuggingFace URI for on-demand download. When included:

- Its SHA-256 digest must appear in `manifest.json` `checksums` as `qmd/model.gguf`.
- Its SHA-256 digest must match `qmd/metadata.json` `embed_model_hash`.

> **Note:** GGUF embedding models are typically 300 MB – 1 GB. Distributors should weigh offline-operation benefits against bundle size when deciding whether to include `model.gguf`.

### 9.6 `qmd/config.json` — Collection Configuration

The QMD collection configuration used to produce the index, expressed as JSON (equivalent to a `qmd.yml` config file). Consumers can use this to reproduce the index or to mount a new QMD store with the same settings.

At minimum this must be a valid JSON object (`{}`). A typical configuration identifies the collection name, path, and glob pattern:

```json
{
  "collections": {
    "aws-sdk": {
      "path": ".",
      "pattern": "**/*.{md,rst,txt}"
    }
  }
}
```

### 9.7 Producing a Scoped QMD Index

QMD's default index is global (`~/.cache/qmd/index.sqlite`). To produce a per-repository index scoped exclusively to one repository, use an explicit `--index` path so the index is never written to the global store.

**CLI approach:**

```sh
# Index and embed the repository into a project-local store
qmd --index ./qmd/index/index.sqlite collection add . --name <bundle-name>
qmd --index ./qmd/index/index.sqlite update
qmd --index ./qmd/index/index.sqlite embed --chunk-strategy auto
```

**SDK approach:**

```js
import { createStore } from '@tobilu/qmd'

const store = await createStore({
  dbPath: './qmd/index/index.sqlite',
  config: {
    collections: {
      [bundleName]: {
        path: repoRoot,
        pattern: '**/*.{md,rst,txt}',
      },
    },
  },
})
await store.update()
await store.embed({ chunkStrategy: 'auto' })
await store.close()
```

The resulting `index.sqlite` is self-contained and portable. Absolute filesystem paths recorded during indexing are not required for search — only the document content and vectors matter at query time.

---

## 10. Versioning and Immutability

- Once packed and published, a CGBundle **must not be modified**. It is treated as an immutable artifact.
- To publish a correction to an already-released version, a new patch version must be issued.
- The `spec_version` field in `manifest.json` identifies which version of this specification the bundle was produced against. Consumers must reject bundles whose `spec_version` major component is higher than the version they support.

**Version compatibility matrix:**

| `spec_version` | Compatible consumers |
|----------------|----------------------|
| `1.x.x`        | Consumers supporting spec `1.x.x` |

---

## 11. Validation Rules

A bundle is **valid** if and only if all of the following hold:

### 11.1 Archive Structure

- [ ] The file is a valid gzip-compressed tar archive.
- [ ] All entries are rooted under a single top-level directory named `<name>-<version>/`.
- [ ] No symlinks or absolute paths are present inside the archive.
- [ ] No path traversal sequences (`../`) appear in any archive entry.

### 11.2 Required Files

- [ ] `manifest.json` is present and is valid JSON.
- [ ] `metadata.json` is present and is valid JSON.
- [ ] `config.json` is present and is valid JSON.
- [ ] `graph/` directory is present and non-empty.
- [ ] `embeddings/` directory is present and non-empty.
- [ ] If `qmd/` is present: `qmd/index/index.sqlite`, `qmd/metadata.json`, `qmd/config.json`, and `qmd/embeddings/` are all present.
- [ ] If `qmd/` is present: `qmd/embeddings/` is non-empty.
- [ ] If `qmd/` is present: `manifest.json` `extensions` includes `"qmd"`.

### 11.3 Manifest Integrity

- [ ] All required fields in `manifest.json` are present and non-empty.
- [ ] `spec_version` is a valid semantic version string.
- [ ] `commit_hash` is a 40-character lowercase hex string.
- [ ] `created_at` is a valid ISO 8601 UTC datetime string.
- [ ] `checksums` contains an entry for every file in the bundle except `manifest.json`.
- [ ] No extra files are present in the bundle that are absent from `checksums`.

### 11.4 Checksum Verification

- [ ] The SHA-256 digest of each listed file matches the value recorded in `checksums`.

### 11.5 Cross-Field Consistency

- [ ] `manifest.json`.`name` matches the filename prefix.
- [ ] `manifest.json`.`version` matches the filename version component.
- [ ] `metadata.json`.`name` matches `manifest.json`.`name`.
- [ ] `metadata.json`.`version` matches `manifest.json`.`version`.
- [ ] `metadata.json`.`source_repo` matches `manifest.json`.`source_repo`.
- [ ] `metadata.json`.`commit_hash` matches `manifest.json`.`commit_hash`.
- [ ] `metadata.json`.`created_at` matches `manifest.json`.`created_at`.
- [ ] If `qmd/` is present: `qmd/metadata.json`.`created_at` matches `manifest.json`.`created_at`.
- [ ] If `qmd/model.gguf` is present: `qmd/metadata.json`.`embed_model_hash` matches the SHA-256 digest of `qmd/model.gguf`.

---

## 12. Error Codes

Implementations should surface validation failures using the following codes:

| Code                     | Description |
|--------------------------|-------------|
| `E_NOT_ARCHIVE`          | File is not a valid gzip tar archive |
| `E_INVALID_ROOT`         | Archive root directory does not match expected `<name>-<version>/` |
| `E_MISSING_FILE`         | A required file or directory is absent |
| `E_INVALID_JSON`         | A required JSON file cannot be parsed |
| `E_MISSING_FIELD`        | A required field is absent from a JSON file |
| `E_INVALID_FIELD`        | A field value fails format validation |
| `E_CHECKSUM_MISMATCH`    | A file's SHA-256 digest does not match the manifest |
| `E_EXTRA_FILE`           | A file is present in the archive but not listed in checksums |
| `E_SYMLINK`              | The archive contains a symbolic link (not permitted) |
| `E_PATH_TRAVERSAL`       | An archive entry contains a path traversal sequence |
| `E_CONSISTENCY_MISMATCH` | Cross-field consistency check failed |
| `E_SPEC_VERSION`         | `spec_version` is unsupported by this consumer |

---

## 13. Reference: Minimal Valid Bundle

The smallest valid bundle contains:

```
aws-sdk-1.11.210/
├── manifest.json
├── metadata.json
├── config.json
├── graph/
│   └── graph.bin
└── embeddings/
    └── vectors.bin
```

With `manifest.json` listing checksums for `metadata.json`, `config.json`, `graph/graph.bin`, and `embeddings/vectors.bin`.

---

## 14. Changelog

| Version | Date       | Notes |
|---------|------------|-------|
| 1.1.0   | 2026-06-12 | Added optional `qmd/` extension (§9) for per-repository QMD documentation index. Added optional `extensions` field to `manifest.json`. Added QMD validation rules to §11.2 and §11.5. |
| 1.0.0   | 2026-06-10 | Initial specification |

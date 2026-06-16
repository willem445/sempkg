# SemBundle Format Specification

**Version:** 1.2.0
**Status:** Draft

---

## 1. Overview

A **SemBundle** (`.SemBundle`) is a portable, immutable, versioned archive that packages a prebuilt [CodeGraph](https://github.com/colbymchenry/codegraph) semantic index for a specific revision of a source repository.

SemBundles allow codebases to publish semantic intelligence artifacts alongside their releases. Consumers — agents, IDEs, and tools — can download and mount a bundle without ever cloning the source repository.

---

## 2. File Naming Convention

```
<name>-<version>.SemBundle
```

| Component | Description |
|-----------|-------------|
| `name`    | Package/library name (lowercase, hyphens allowed, no spaces) |
| `version` | Semantic version (`MAJOR.MINOR.PATCH`) or tag identifier |

**Examples:**

```
aws-sdk-1.11.210.SemBundle
qt-6.7.0.SemBundle
ros2-humble.SemBundle
```

A `.SemBundle` file is a **gzip-compressed tar archive** (`.tar.gz` renamed to `.SemBundle`). All paths inside the archive are relative and rooted at a single top-level directory matching the bundle name:

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
└── lance/              # LanceDB documentation index (optional)
    ├── metadata.json   # Lance index metadata
    └── docs.lance/     # LanceDB table directory (Arrow/Lance files)
        └── ...         # Arrow Lance data and index files
```

The top-level entries `manifest.json`, `metadata.json`, `config.json`, `graph/`, and `embeddings/` are **required**. The `lance/` entry is **optional**. A bundle missing any required entry is invalid.

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
| `spec_version`      | string          | Yes      | SemBundle spec version this bundle conforms to (e.g. `"1.0.0"`) |
| `name`              | string          | Yes      | Package name. Must match the filename prefix. Pattern: `^[a-z0-9][a-z0-9\-]*[a-z0-9]$` |
| `version`           | string          | Yes      | Package version. Must match the filename version component. |
| `source_repo`       | string          | Yes      | Canonical URL of the source repository (e.g. `"https://github.com/org/repo"`) |
| `commit_hash`       | string          | Yes      | Full 40-character Git SHA of the indexed commit |
| `tag`               | string or null  | Yes      | Git tag associated with this version, or `null` if none |
| `created_at`        | string          | Yes      | ISO 8601 UTC timestamp of when the bundle was created (e.g. `"2025-08-01T14:30:00Z"`) |
| `codegraph_version` | string          | Yes      | Version of CodeGraph used to produce the index |
| `extensions`        | array of string | No       | Optional bundle extensions present. Must include `"lance"` when `lance/` is present. Omit or use `[]` when no extensions are bundled. |
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

## 9. `lance/` — Documentation Index

The `lance/` directory is an **optional** extension that bundles a per-repository documentation index stored in [LanceDB](https://lancedb.github.io/lancedb/) format. LanceDB is a pure-Rust, embedded, serverless vector database that stores data as portable Arrow Lance files — no server process, no global state, no Python runtime required.

When present, `lance/` allows consumers (agents, IDEs, MCP clients) to query the repository's documentation offline using BM25 full-text search and (optionally) vector semantic search. The index is scoped exclusively to the repository being bundled.

When `lance/` is present, `manifest.json` must include `"lance"` in its `extensions` array.

### 9.1 Sub-directory Layout

```
lance/
├── metadata.json      # Lance index metadata (required)
└── docs.lance/        # LanceDB table directory (required)
    └── ...            # Arrow Lance data files and tantivy FTS index
```

`metadata.json` and at least one `*.lance/` table directory are **required** when `lance/` is present.

### 9.2 `lance/docs.lance/` — LanceDB Table

The `docs.lance/` directory is a standard LanceDB table directory containing Arrow Lance data files and an optional tantivy BM25 full-text search index. It is produced by running `lancedb::connect()` against the `lance/` directory and creating a table named `docs`.

**Table schema:**

| Column    | Type   | Description |
|-----------|--------|-------------|
| `path`    | Utf8   | Document path (relative to repo root, with optional `#<chunk-index>` suffix) |
| `content` | Utf8   | Document chunk text content |

Consumers open the table by connecting to the `lance/` directory:

```rust
// Rust (lancedb crate)
let db = lancedb::connect(bundle_dir.join("lance").to_str().unwrap()).execute().await?;
let table = db.open_table("docs").execute().await?;
let results = table.query()
    .full_text_search(lancedb::query::FullTextSearchQuery::new("authentication flow".to_owned()))
    .limit(10)
    .execute().await?;
```

```python
# Python (lancedb package)
import lancedb
db = lancedb.connect(str(bundle_dir / "lance"))
table = db.open_table("docs")
results = table.search("authentication flow", query_type="fts").limit(10).to_list()
```

### 9.3 `lance/metadata.json` — Index Metadata

#### Schema

```json
{
  "table_name":      "<string>",
  "document_count":  "<number>",
  "chunk_count":     "<number>",
  "indexed_paths":   ["<string>", ...],
  "fts_enabled":     "<boolean>",
  "created_at":      "<ISO 8601 UTC datetime>"
}
```

#### Field Definitions

| Field            | Type            | Required | Description |
|------------------|-----------------|----------|-------------|
| `table_name`     | string          | Yes      | LanceDB table name (always `"docs"`) |
| `document_count` | number          | Yes      | Number of source documents indexed |
| `chunk_count`    | number          | Yes      | Total number of text chunks (rows in the table) |
| `indexed_paths`  | array of string | Yes      | Glob patterns or directories indexed |
| `fts_enabled`    | boolean         | Yes      | Whether a tantivy BM25 FTS index was built |
| `created_at`     | string          | Yes      | ISO 8601 UTC timestamp (must match `manifest.json` `created_at`) |

#### Example

```json
{
  "table_name": "docs",
  "document_count": 1842,
  "chunk_count": 9231,
  "indexed_paths": ["docs/**/*.md", "**/*.rst", "README.md"],
  "fts_enabled": true,
  "created_at": "2025-08-01T14:30:00Z"
}
```

### 9.4 Producing a LanceDB Index

The recommended tool is the `SemBundle build --docs-dir <path>` command, which walks documentation directories, chunks the text, writes a LanceDB `docs` table, and builds a tantivy BM25 full-text search index — all in-process with no external tool dependency.

For manual indexing:

```rust
use lancedb::{connect, index::Index, index::scalar::FtsIndexBuilder};
use arrow_array::{RecordBatch, RecordBatchIterator, StringArray};
use arrow_schema::{DataType, Field, Schema};
use std::sync::Arc;

let db = connect("./lance").execute().await?;
let schema = Arc::new(Schema::new(vec![
    Field::new("path", DataType::Utf8, false),
    Field::new("content", DataType::Utf8, false),
]));
let batch = RecordBatch::try_new(schema.clone(), vec![
    Arc::new(StringArray::from(paths)),
    Arc::new(StringArray::from(contents)),
])?;
let reader = RecordBatchIterator::new(vec![Ok(batch)], schema);
let tbl = db.create_table("docs", reader).execute().await?;
tbl.create_index(&["content"], Index::FTS(FtsIndexBuilder::default()))
    .execute().await?;
```

---

## 10. Versioning and Immutability

- Once packed and published, a SemBundle **must not be modified**. It is treated as an immutable artifact.
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
- [ ] If `lance/` is present: `lance/metadata.json` is present and is valid JSON.
- [ ] If `lance/` is present: at least one `*.lance/` table directory is present inside `lance/`.
- [ ] If `lance/` is present: `manifest.json` `extensions` includes `"lance"`.

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
- [ ] If `lance/` is present: `lance/metadata.json`.`created_at` matches `manifest.json`.`created_at`.

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
| 1.2.0   | 2026-06-15 | Replaced `qmd/` extension with `lance/` (LanceDB Arrow files). Updated §3 layout, rewrote §9, updated §11.2 and §11.5 validation rules. Bumped `spec_version`. |
| 1.1.0   | 2026-06-12 | Added optional `qmd/` extension (§9) for per-repository QMD documentation index. Added optional `extensions` field to `manifest.json`. Added QMD validation rules to §11.2 and §11.5. |
| 1.0.0   | 2026-06-10 | Initial specification |

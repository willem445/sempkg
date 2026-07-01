# sembundle — Developer Guide

`sembundle` is a Rust CLI that packs CodeGraph output directories into portable
`.sembundle` archives (gzip-compressed tar files).

---

## 1. Prerequisites

### 1.1 Rust toolchain

Install Rust via [rustup](https://rustup.rs). This installs the compiler
(`rustc`), package manager (`cargo`), and standard toolchain.

**Windows (PowerShell):**

```powershell
# Download and run the rustup installer
Invoke-WebRequest -Uri https://win.rustup.rs -OutFile rustup-init.exe
.\rustup-init.exe
```

During the installer prompts:
- Accept the default installation (`1 — Proceed with standard installation`).
- When complete, **close and reopen your terminal** so that `cargo` and `rustc`
  are on `PATH`.

Verify:

```powershell
rustc --version   # e.g. rustc 1.78.0 (9b00956e5 2024-04-29)
cargo --version   # e.g. cargo 1.78.0 (ba5b50945 2024-05-20)
```

### 1.2 C/C++ linker (Windows only)

Rust on Windows requires a compatible C linker. The two options are:

| Option | Instructions |
|--------|-------------|
| **MSVC** (recommended) | Install [Visual Studio Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/) and select the **"Desktop development with C++"** workload. |
| **GNU (MinGW-w64)** | Install [MSYS2](https://www.msys2.org/), then `pacman -S mingw-w64-ucrt-x86_64-gcc`, and add `C:\msys64\ucrt64\bin` to PATH. |

rustup will warn you if no linker is found on first build.

---

## 2. Crate Dependencies

All dependencies are declared in [Cargo.toml](Cargo.toml) and downloaded
automatically by `cargo`. No manual installation is needed.

| Crate | Version | Purpose |
|-------|---------|---------|
| `clap` | 4 | CLI argument parsing (`derive` feature for struct-based flags) |
| `serde` | 1 | Serialization framework (`derive` feature for `#[derive(Serialize, Deserialize)]`) |
| `serde_json` | 1 | JSON serialization/deserialization |
| `sha2` | 0.10 | SHA-256 digest computation |
| `hex` | 0.4 | Encode byte arrays as lowercase hex strings |
| `tar` | 0.4 | Building and reading tar archives |
| `flate2` | 1 | Gzip compression/decompression (wraps `tar` streams) |
| `chrono` | 0.4 | UTC timestamp generation for `created_at` fields |
| `walkdir` | 2 | Recursive directory traversal |
| `thiserror` | 1 | Ergonomic `Error` trait derivation for `PackError` || `lancedb` | 0.14 | LanceDB embedded database — used by `build.rs` to create the `lance/` doc index |
| `arrow-array` | 52 | Apache Arrow columnar arrays — data rows for the LanceDB table |
| `arrow-schema` | 52 | Arrow schema definition for the `docs` table (`path`, `content` columns) |
| `tokio` | 1 | Async runtime — LanceDB's Rust API is fully async; `block_on` is used from sync entry points |
| `glob` | 0.3 | Glob pattern matching for `--docs-glob` file discovery |
| `which` | 6 | Locate `codegraph` on PATH |
**Dev dependencies** (tests only):

| Crate | Version | Purpose |
|-------|---------|---------|
| `tempfile` | 3 | Create isolated temporary directories for test fixtures |

---

## 3. Project Structure

```
src/sembundle/
├── Cargo.toml          # Package manifest and dependency declarations
├── DEV-GUIDE.md        # This file
└── src/
    ├── main.rs         # CLI entry point (clap subcommands: pack, build, publish, sign, verify, keygen)
    ├── pack.rs         # Core pack logic + integration tests
    ├── build.rs        # Build pipeline: codegraph + LanceDB indexing + pack
    ├── manifest.rs     # Manifest, Metadata, LanceMetadata struct definitions
    ├── checksum.rs     # SHA-256 helpers + unit tests
    ├── validate.rs     # Input validation + unit tests (validate_lance_dir)
    └── error.rs        # PackError enum (thiserror)
```

---

## 4. Build

All commands must be run from the `src/sembundle/` directory.

```powershell
cd c:\Projects\sempkg\src\sembundle
```

### Debug build (fast compile, includes debug symbols)

```powershell
cargo build
# Binary: target\debug\sembundle.exe
```

### Release build (optimized, for distribution)

```powershell
cargo build --release
# Binary: target\release\sembundle.exe
```

---

## 5. Run Tests

```powershell
cargo test
```

This runs all unit tests (in `checksum.rs`, `validate.rs`) and all integration
tests (in `pack.rs`) in a single pass. Expected output:

```
running 18 tests
test checksum::tests::known_vector ... ok
test checksum::tests::empty_input ... ok
test checksum::tests::distinct_inputs_produce_distinct_digests ... ok
test checksum::tests::same_input_is_deterministic ... ok
test validate::tests::valid_names ... ok
test validate::tests::name_too_short ... ok
test validate::tests::name_starts_with_hyphen ... ok
test validate::tests::name_ends_with_hyphen ... ok
test validate::tests::name_uppercase_rejected ... ok
test validate::tests::name_with_spaces_rejected ... ok
test validate::tests::valid_commit_hash ... ok
test validate::tests::short_hash_rejected ... ok
test validate::tests::uppercase_hash_rejected ... ok
test validate::tests::hash_with_non_hex_rejected ... ok
test pack::tests::pack_succeeds_with_valid_input ... ok
test pack::tests::error_when_graph_dir_missing ... ok
...
test result: ok. 18 passed; 0 failed
```

---

## 6. Usage

There are two ways to create a bundle:

- **`sembundle build`** — run CodeGraph indexing and (optionally) LanceDB documentation indexing, then pack everything in one step. Recommended for CI.
- **`sembundle pack`** — pack a pre-existing CodeGraph output directory. Use this if you already have a `graph/` directory and want to add a pre-built `lance/` extension.

---

### `sembundle build` — Full pipeline (recommended)

```
sembundle build --name <name> --version <ver> --source-repo <url> --commit-hash <sha>
               --codegraph-version <ver> --source-dir <dir> [--source-dir <dir> ...]
               [--docs-dir <dir> [--docs-dir <dir> ...] [--docs-glob <pattern>]]
               [--tag <tag>] [--language <lang>] [--output <path>]
```

| Flag | Required | Description |
|------|----------|-------------|
| `--name` / `-n` | Yes | Package name |
| `--version` / `-r` | Yes | Version string |
| `--source-repo` | Yes | Canonical repository URL |
| `--commit-hash` | Yes | Full 40-character Git SHA |
| `--codegraph-version` | Yes | Version of CodeGraph used |
| `--source-dir` / `-s` | Yes (repeat) | Source directory to index with `codegraph init --index`. Multiple flags allowed. |
| `--docs-dir` / `-d` | No (repeat) | Documentation directory to index with LanceDB. Multiple flags allowed. Omit to skip the `lance/` extension. |
| `--docs-glob` | No | Glob pattern for doc discovery. Default: `**/*.{md,txt,rst}`. Comma-separate multiple patterns. |
| `--tag` | No | Git tag |
| `--language` | No | Primary language (default: `unknown`) |
| `--output` / `-o` | No | Output `.sembundle` path (default: `./<name>-<version>.sembundle`) |

**Example — source only:**

```powershell
sembundle build `
  --name aws-sdk `
  --version 1.11.210 `
  --source-repo https://github.com/aws/aws-sdk-cpp `
  --commit-hash d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3 `
  --codegraph-version 0.3.1 `
  --source-dir C:\Projects\aws-sdk\src `
  --language cpp
```

**Example — source + LanceDB documentation index:**

```powershell
sembundle build `
  --name my-sdk `
  --version 2.0.0 `
  --source-repo https://github.com/org/my-sdk `
  --commit-hash a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0 `
  --codegraph-version 0.3.1 `
  --source-dir C:\Projects\my-sdk\src `
  --docs-dir   C:\Projects\my-sdk\docs `
  --docs-glob  "**/*.md" `
  --language   rust
```

The build step:
1. Runs `codegraph init --index` on each `--source-dir`
2. Walks each `--docs-dir`, matches files against `--docs-glob`, chunks them into ~800-char paragraphs, writes a LanceDB `docs` table, and creates a tantivy BM25 FTS index — all in-process, no external tools
3. Packs everything into a `.sembundle` archive

---

### `sembundle pack` — Pack a pre-existing CodeGraph output directory

```
sembundle pack <INPUT_DIR> [OPTIONS]
```

### Required arguments

| Argument | Flag | Description |
|----------|------|-------------|
| Input directory | (positional) | Path to a CodeGraph output directory containing `graph/`, `embeddings/`, `config.json` |
| Name | `--name` / `-n` | Package name (lowercase letters, digits, hyphens; ≥2 chars) |
| Version | `--version` / `-r` | Version string (e.g. `1.2.3`, `humble`) |
| Source repo | `--source-repo` | Canonical repository URL |
| Commit hash | `--commit-hash` | Full 40-character lowercase Git SHA |
| CodeGraph version | `--codegraph-version` | Version of CodeGraph used to build the index |

### Optional arguments

| Flag | Default | Description |
|------|---------|-------------|
| `--tag` | none | Git tag associated with this release |
| `--language` | `unknown` | Primary language indexed (e.g. `python`, `cpp`, `rust`) |
| `--indexed-paths` | `.` | Comma-separated list of repo-relative paths that were indexed |
| `--output` / `-o` | `./<name>-<version>.sembundle` | Output file path |
| `--lance-dir` | none | Path to a pre-built LanceDB directory to include as the `lance/` extension. Must contain `metadata.json` and a `docs.lance/` subdirectory. |

---

## 7. Adding the binary to PATH (optional)

After a release build, copy the binary to a directory on your PATH:

```powershell
# Example: add to a local tools directory
Copy-Item .\target\release\sembundle.exe $env:USERPROFILE\bin\sembundle.exe
```

Or run it directly:

```powershell
.\target\release\sembundle.exe pack ...
```

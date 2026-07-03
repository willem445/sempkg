# Implementation Plan: Local Datasheet (PDF → Markdown) Indexing

> **Status:** Proposed
> **Audience:** Implementation model / engineer
> **Goal:** Let a user point `sembundle` / `sempkg` at a folder of **local PDF
> datasheets** (or other docs), convert the PDFs to Markdown **locally**, build a
> docs-only `.sembundle`, and install it into a workspace — with no source code,
> no git repo, and no registry. Hosting on a registry stays possible later with
> zero format changes.

---

## 1. Objective

Today a `.sembundle` is fundamentally **source-code centric**:

- The build pipeline shells out to `codegraph` against `--source-dir`, and the
  resulting `graph/` + `embeddings/` directories are **required** bundle entries
  ([validate.rs](../src/sembundle/src/validate.rs) `REQUIRED_DIRS`).
- `manifest.commit_hash` must be a 40-char git SHA
  ([validate.rs](../src/sembundle/src/validate.rs) `validate_commit_hash`).
- The optional `lance/` docs index only indexes already-textual files
  (`**/*.md,**/*.txt,**/*.rst` — [build.rs](../src/sembundle/src/build.rs)
  `run_lance`).

A folder of PDF datasheets has **no source code, no git history, and no Markdown**.
So two capabilities are missing:

1. **Docs-only bundles** — a bundle whose only payload is the `lance/` docs index
   (no `graph/`, no `embeddings/`, no git commit).
2. **Local PDF → Markdown conversion** — turn `*.pdf` into Markdown before the
   LanceDB indexing step.

End-to-end target:

```bash
# sembundle: build a docs-only bundle from a folder of datasheets.
sembundle build-docs \
  --name stm32-datasheets --version 2024.1 \
  --docs-dir ./datasheets \
  --convert-pdf

# sempkg: index local datasheets and install into the workspace in one step.
sempkg add ./datasheets --name stm32-datasheets --docs-only --convert-pdf
```

Then an agent connected to the `sempkg` MCP server can call `search_docs` /
`docs_metadata` against the datasheet corpus exactly as it does for any other
bundle's docs index. All source-code tools (`search_symbols`, `get_callers`, …)
simply report "no code index in this bundle".

Everything is **opt-in and additive**: existing bundles, builds, and installs are
unchanged.

---

## 2. Current Architecture (context for the implementer)

- `src/sembundle/` — packs / builds bundles.
  - [build.rs](../src/sembundle/src/build.rs): `build(BuildOptions)` →
    `run_codegraph` (required) → `run_lance` (optional docs) → `run_source_index`
    (optional code) → `pack`.
  - [pack.rs](../src/sembundle/src/pack.rs): `pack(PackOptions)` validates the
    input dir, copies `graph/`+`embeddings/`+`config.json`, generates
    `metadata.json` / `manifest.json`, and appends `lance` / `code` extensions.
    `spec_version` is `1.2.0` (→ `1.3.0` when `code/` present).
  - [validate.rs](../src/sembundle/src/validate.rs): `validate_input_dir`
    (requires non-empty `graph/`+`embeddings/`+`config.json`),
    `validate_commit_hash` (40-hex), `validate_lance_dir`.
  - [manifest.rs](../src/sembundle/src/manifest.rs): `Manifest`, `Metadata`,
    `LanceMetadata`, `CodeMetadata`.
  - [lib.rs](../src/sembundle/src/lib.rs): re-exports `build`, `BuildOptions`,
    `pack`, `PackOptions` (consumed by `sempkg`).
- `src/sempkg/` — manager + MCP server.
  - [main.rs](../src/sempkg/src/main.rs): `add_from_local(...)` builds a bundle
    from a local folder via `sembundle::build` and installs it; records a
    `{ local = "...", version = "..." }` entry in `sempkg.toml`.
  - [store.rs](../src/sempkg/src/store.rs): `install_bytes`, and
    `create_codegraph_view` which **already silently skips when `graph/` is
    absent** (line ~324) — so a docs-only bundle installs cleanly today.
  - [lance.rs](../src/sempkg/src/lance.rs): `search` (BM25 over `docs` table),
    `has_lance`. The query side needs **no change** for docs-only bundles.
  - [mcp.rs](../src/sempkg/src/mcp.rs): `search_docs` / `docs_metadata` already
    operate purely on `lance/`; codegraph tools error gracefully when not indexed.

**Key observation:** the *read* path already supports docs-only bundles — the
LanceDB docs index is self-contained and codegraph access already degrades
gracefully. The genuinely new work is on the **build/pack** side: (a) allow a
bundle with no `graph/`+`embeddings/`, (b) relax `commit_hash`, and (c) add a
PDF → Markdown conversion stage feeding the existing `run_lance`.

---

## 3. Design Decisions

### 3.1 Bundle format: docs-only bundles (`spec_version` 1.4.0)

Relax the "required entries" rule so a bundle is valid when it carries **at least
one** payload index:

| Payload | Directory | Producer |
|---|---|---|
| Code graph | `graph/` + `embeddings/` | codegraph (existing) |
| Docs index | `lance/` | LanceDB (existing) |
| Code index | `code/` | LanceDB (existing) |

New rule: a bundle must contain **`config.json`** plus **at least one** of
`{ graph/+embeddings/, lance/, code/ }`. `graph/` and `embeddings/` become a
matched optional pair (present together or absent together).

- Bump `spec_version` to **`1.4.0`**.
- Add a `bundle_kind` field to `manifest.json` (and mirror in `metadata.json`):
  - `"code"` — has `graph/`+`embeddings/` (today's default).
  - `"docs"` — docs-only (no graph/embeddings).
  - `"mixed"` — has both code graph and docs/code indexes.
  - Older bundles without the field are treated as `"code"` for back-compat.
- `commit_hash` becomes **optional / nullable** (`Option<String>`), and the
  40-hex check only applies when it is present. Datasheets have no commit; the
  field is `null`. (Spec §4.2 / §5 updated accordingly.)
- `source_repo` for a local datasheet build is recorded as
  `local:<abs-path>` (mirrors the existing local-source convention in
  [main.rs](../src/sempkg/src/main.rs) `add_from_local`).

> Rationale: a new nullable field + relaxed presence rule is the smallest change
> that keeps all existing readers working (unknown extensions/fields ignored) and
> avoids inventing a second archive format.

### 3.2 PDF → Markdown conversion

Datasheets are layout-heavy (multi-column, tables, figures). Conversion quality
varies a lot by tool, so make the converter **pluggable** with a safe default,
mirroring the existing "shell out to `codegraph`" pattern.

**Two-tier strategy:**

1. **Built-in (default, zero external deps):** pure-Rust text extraction via the
   [`pdf-extract`](https://crates.io/crates/pdf-extract) crate. Produces plain
   text wrapped as Markdown (one `#` heading per file, page breaks as `---`).
   Good enough for BM25 search over datasheet *text*; weak on tables/layout.
2. **External converter (opt-in, high quality):** `--pdf-converter <cmd>` runs an
   external tool per file and captures Markdown on stdout (or an output file).
   Tested targets: `markitdown`, `marker`, `docling`, `pymupdf4llm`. A `{input}`
   / `{output}` placeholder convention is used, e.g.:

   ```bash
   --pdf-converter "markitdown {input}"            # stdout → markdown
   --pdf-converter "marker_single {input} {output_dir}"
   ```

   When set, the built-in extractor is bypassed. If the external command is
   missing or fails for a file, log a warning and fall back to the built-in
   extractor (configurable: `--pdf-strict` to fail instead).

**Where conversion happens:** a new `convert_pdfs(...)` stage runs **before**
`run_lance`. It walks the docs dirs for `*.pdf`, writes the generated `*.md`
into a temp "converted docs" directory that preserves the relative tree, and the
existing `run_lance` then indexes that temp dir (plus any pre-existing
`*.md/.txt` left in place). The converted Markdown is **not** added to the bundle
verbatim — only the LanceDB index is — keeping bundles lean (the original PDFs
stay on the user's disk). An `--keep-markdown <dir>` flag optionally writes the
converted Markdown out for inspection.

> Decision: default to built-in `pdf-extract` so `--convert-pdf` works with no
> external install; recommend `--pdf-converter` for production-quality datasheet
> tables. This keeps the "works offline, no cloud" guarantee.

### 3.3 Chunking

Reuse the existing `chunk_text(text, 800)` paragraph chunker in
[build.rs](../src/sembundle/src/build.rs) `run_lance`. Datasheet Markdown chunks
cleanly on blank lines. Optionally add a `--docs-chunk-size` flag (default 800)
since datasheet sections can be long; not required for v1.

### 3.4 LanceDB schema — unchanged

The docs table stays `(path, content)` with an FTS index on `content`
(spec §9.2). `path` uses the converted Markdown's repo-relative path with the
existing `#<chunk-index>` suffix. No reader changes required.

---

## 4. Implementation Steps

### Step 1 — `sembundle`: PDF conversion module
- **New** `src/sembundle/src/pdf.rs`:
  - `pub struct PdfConvertOptions { converter_cmd: Option<String>, strict: bool, keep_markdown: Option<PathBuf> }`.
  - `fn convert_pdfs(docs_dirs, out_dir, opts, exclude_dirs) -> Result<usize>`:
    walk for `*.pdf`, convert each to Markdown under `out_dir` preserving the
    relative tree, return the count converted.
  - `fn convert_one_builtin(pdf_path) -> Result<String>`: `pdf-extract` →
    text → light Markdown wrapping (title heading + `---` page separators).
  - `fn convert_one_external(cmd, pdf_path, out_path) -> Result<String>`:
    substitute `{input}`/`{output}` placeholders, run, capture Markdown.
  - Add `pdf-extract = "0.9"` (or current) to
    [Cargo.toml](../src/sembundle/Cargo.toml).

### Step 2 — `sembundle`: docs-only build pipeline
- [build.rs](../src/sembundle/src/build.rs):
  - Extend `BuildOptions` with:
    - `docs_only: bool` (skip codegraph entirely),
    - `convert_pdf: bool`,
    - `pdf_converter: Option<String>`, `pdf_strict: bool`, `keep_markdown: Option<PathBuf>`,
    - `commit_hash: Option<String>` (change type from `String`).
  - In `build()`:
    - When `docs_only`, **skip** `run_codegraph`; require at least one
      `docs_dir`. Otherwise behave as today.
    - When `convert_pdf` (or any `*.pdf` present), run `pdf::convert_pdfs` into a
      temp dir and **prepend** it to the set of docs dirs passed to `run_lance`.
    - Pass `code_dir: None` / `lance_dir: Some(...)` and a `kind` to `pack`.

### Step 3 — `sembundle`: pack + manifest + validate
- [manifest.rs](../src/sembundle/src/manifest.rs):
  - `Manifest.commit_hash: Option<String>`; add `bundle_kind: String`.
  - `Metadata.commit_hash: Option<String>`; add `bundle_kind: String`.
  - Bump default `spec_version` to `"1.4.0"`.
- [pack.rs](../src/sembundle/src/pack.rs):
  - Add `pub bundle_kind: BundleKind` (or `docs_only: bool`) + make
    `input_dir: Option<PathBuf>` (None for docs-only).
  - Skip `validate_input_dir` and the `graph/`+`embeddings/`+`config.json` copy
    when there is no `input_dir`; instead write a minimal `config.json` (`{}`).
  - Only call `validate_commit_hash` when `commit_hash` is `Some`.
  - Require at least one of `{ input_dir, lance_dir, code_dir }`; else error.
  - Set `bundle_kind` / `spec_version` accordingly.
- [validate.rs](../src/sembundle/src/validate.rs):
  - Make `validate_input_dir` only called for code bundles (no signature change;
    just gate the call site).
  - `validate_commit_hash` unchanged (only invoked when `Some`).

### Step 4 — `sembundle`: CLI surface
- [main.rs](../src/sembundle/src/main.rs):
  - **New** subcommand `build-docs` (docs-only convenience), or add
    `--docs-only` to `build`. Prefer a dedicated `build-docs` so required-arg
    rules differ cleanly (no `--source-dir`, no `--commit-hash`):
    - `--name`, `--version` (required); `--docs-dir` (>=1, required);
      `--docs-glob`, `--convert-pdf`, `--pdf-converter`, `--pdf-strict`,
      `--keep-markdown`, `--exclude-dir`, `--source-repo` (optional, default
      `local:<docs-dir>`).
  - Wire into `build::build` with `docs_only: true`, `commit_hash: None`.

### Step 5 — `sempkg`: local docs-only add
- [cli.rs](../src/sempkg/src/cli.rs) `Add`:
  - Add `--docs-only` (build a docs index from a local folder, no codegraph),
    `--convert-pdf`, `--pdf-converter <cmd>`, `--pdf-strict`,
    `--keep-markdown <dir>`.
- [main.rs](../src/sempkg/src/main.rs) `add_from_local(...)`:
  - Thread the new flags into `sembundle::BuildOptions` with `docs_only`,
    `convert_pdf`, `pdf_converter`, `commit_hash: None` for docs-only.
  - When `--docs-only`, default `docs_dirs` to the folder and **do not** set
    `source_dirs` (no codegraph run).
  - Auto-detect convenience: if a folder contains only PDFs/docs and no code,
    suggest `--docs-only` in the error/help (do not silently switch).
- `record_local_dep(...)`: persist `docs_only` / `convert_pdf` / `pdf_converter`
  in the `sempkg.toml` dependency entry so `sempkg refresh` / `sync` rebuild
  identically (mirror existing `include_source` / `source_glob` handling).
- [manifest.rs](../src/sempkg/src/manifest.rs) `DependencyEntry`: add the new
  optional fields.

### Step 6 — `sempkg`: read-path robustness (mostly free)
- [store.rs](../src/sempkg/src/store.rs): confirm `create_codegraph_view` skips
  when `graph/` absent (it does) and `is_indexed()` returns `false` for
  docs-only bundles. Surface a `+docs`-only marker in `sempkg list` (the bundle
  shows `+lance` already via `has_lance()`).
- [mcp.rs](../src/sempkg/src/mcp.rs): ensure codegraph-backed tools return a
  clean "this bundle has no code index" message (not a hard error) when
  `!is_indexed()`. `search_docs` / `docs_metadata` already work unchanged.

### Step 7 — Spec + docs
- [sembundle-spec.md](sembundle-spec.md):
  - Bump to `1.4.0`; document `bundle_kind`; make `commit_hash` nullable;
    redefine §3/§11 "required entries" as "config.json + at least one payload
    index"; note docs-only bundles.
- [sembundle.md](sembundle.md) / [sempkg.md](sempkg.md): document `build-docs`,
  `--docs-only`, `--convert-pdf`, `--pdf-converter`, and the datasheet workflow.
- README: short "Index local PDFs/datasheets" example.

### Step 8 — Tests
- `sembundle`:
  - `pdf::convert_pdfs` against a tiny fixture PDF → asserts non-empty Markdown.
  - Docs-only `pack`/`build` round-trip: extract → assert no `graph/`/`embeddings/`,
    `bundle_kind == "docs"`, `commit_hash == null`, `extensions` contains
    `"lance"`, checksums verify, validation passes.
  - Back-compat: a code bundle still builds with `bundle_kind == "code"` and
    `spec_version` consistent; a `1.2.0`/`1.3.0` bundle still validates.
- `sempkg`:
  - `add ./fixtures/datasheets --docs-only --convert-pdf` installs; `search_docs`
    returns a seeded datasheet term; codegraph tools report "no code index".

---

## 5. Backwards Compatibility

- Purely additive. Existing code bundles keep `graph/`+`embeddings/`, gain a
  `bundle_kind: "code"` field, and otherwise build identically.
- `spec_version` bump to `1.4.0`; validators accept `commit_hash: null` and a
  missing `graph/`/`embeddings/` **only** when another payload index is present.
- Consumers on spec `1.x` ignore the unknown `bundle_kind` field; the docs read
  path is unchanged.
- `--convert-pdf` with no external converter uses the built-in `pdf-extract`
  path, so the feature works on a clean machine with no extra installs.

---

## 6. Open Questions / Decisions

1. **CLI shape:** dedicated `sembundle build-docs` subcommand vs. `--docs-only`
   flag on `build`. Proposed: dedicated subcommand (cleaner required-arg rules).
2. **Default PDF converter:** built-in `pdf-extract` (proposed) vs. requiring an
   external high-quality tool. Built-in keeps zero-dep operation; document the
   `--pdf-converter` upgrade path for table-heavy datasheets.
3. **`commit_hash` for docs:** `null` (proposed) vs. a synthetic content hash of
   the source PDFs (would give reproducibility/dedup but complicates the 40-hex
   spec rule). Proposed: `null` for v1, content-hash as a follow-up.
4. **Vector search:** datasheets benefit from semantic (not just BM25) retrieval.
   v1 uses FTS + the existing reranker; embedding the docs table is a follow-up
   (same decision the source-code index plan made).
5. **OCR:** scanned/image-only datasheet PDFs need OCR. Out of scope for v1;
   `--pdf-converter` with an OCR-capable tool (e.g. `marker`, `docling`) is the
   escape hatch.

---

## 7. Suggested Sequencing

1. **Step 1–4** (`sembundle`: docs-only `pack`/`build` + PDF conversion) — an
   independently shippable slice; verify with
   `sembundle build-docs --docs-dir ./datasheets --convert-pdf` + manual extract.
2. **Step 5–6** (`sempkg`: `add --docs-only --convert-pdf` + read-path checks).
3. **Step 7–8** (spec, docs, tests) alongside each stage.
4. **Later (registry hosting):** no format work needed — a docs-only `.sembundle`
   publishes through the existing `sembundle publish` / registry path as-is.

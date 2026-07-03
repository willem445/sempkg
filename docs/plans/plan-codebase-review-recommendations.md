# sempkg In-Depth Review — Recommendations Plan

## Context

Full review of the sempkg repo (~18.5k first-party lines: `src/sempkg` Rust CLI+MCP server, `src/sembundle` Rust bundler, `src/sempkg_registry` Python FastAPI registry, plus the undocumented fourth app `src/sempkg_agent`). Three parallel review agents covered each area; the highest-impact claims were verified by direct inspection. Overall the codebase is fundamentally healthy — leaf modules are clean and unit-tested (~65 Rust tests in sempkg, ~40 in sembundle, solid TokenStore/BundleStorage tests in Python). The problems cluster into: **cross-crate duplication of the `.sembundle` format contract**, **a hot-path performance problem in the MCP `query` pipeline**, **a broken registry Docker image and untested HTTP layer**, and **god-files + CI gaps**.

Findings are grouped into 6 workstreams. Each is independently executable and sized for a single agent. Recommended order: WS1 → WS2 → WS3 → WS4 → WS5 → WS6, but WS2/WS3/WS4 have no dependency on WS1 and can run in parallel with it.

---

## WS1 — Cargo workspace + shared `.sembundle` format layer (HIGH, correctness + reuse)

There is no root `Cargo.toml`; the two Rust crates are independent, with ~20 dependency versions pinned twice and feature drift already present (`reqwest`, `tokio`, `ed25519-dalek`). The `.sembundle` format contract (manifest schema, signing convention, archive layout, checksums) is implemented twice — writer in sembundle, reader in sempkg — and can silently diverge.

1. **Create a cargo workspace** at repo root with `[workspace.dependencies]`; convert both member manifests to `dep = { workspace = true }`. Unifies lockfile/`target/`, eliminates version/feature drift (esp. `ed25519-dalek`, where a version split would break signature verification across the crate boundary).
2. **Widen `sembundle`'s library surface** (no new crate needed — sempkg already links it via `path = "../sembundle"` but only uses `build`/`BuildOptions`):
   - Promote `verify`, `sign`, `keygen` from binary-private (`sembundle/src/main.rs:6-10`) into `lib.rs`.
   - Add archive reader helpers: `read_manifest(bytes)`, `verify_checksums(bytes, &manifest)`, a bundle-relative entry iterator. Centralize magic dir/file-name strings (`"manifest.json"`, `"graph"`, `"embeddings"`, `"lance"`, `"code"`, spec versions `"1.2.0"/"1.3.0"` at `pack.rs:134-138`) as `pub const`s.
3. **Delete sempkg's duplicates and delegate:**
   - `BundleManifest` (`sempkg/src/store.rs:32-55`) is field-identical to `sembundle::manifest::Manifest` → `pub use` the shared type; keep `has_lance()`/`has_code()` as an extension trait.
   - `sempkg/src/verify.rs` Ed25519 logic → delegate to `sembundle::verify` (one place owns "Ed25519 over hex(SHA256(bytes))", including the `.sig` hex-decode convention).
   - 5 inline `hex::encode(Sha256::digest(...))` sites (`registry.rs:108,228`, `verify.rs:32`, `store.rs:155,472`) → `sembundle::checksum::sha256_bytes`.
   - `store.rs:413-484` (`read_manifest_from_tar`, `validate_checksums`, prefix-stripping) → the new shared reader helpers.
4. **Reproducibility fix:** `Manifest.checksums` is a `HashMap` (`sembundle/src/manifest.rs:25`) serialized into the signed manifest — iteration order is nondeterministic, so identical inputs produce different signed bytes. Change to `BTreeMap` (sempkg's copy already uses one).
5. **Add a keygen → sign → verify round-trip test** in the shared layer so writer/reader convention drift is caught by CI.

## WS2 — MCP `query` hot-path performance (HIGH, speed)

The `query` tool's cost is dominated by avoidable per-call setup, compounding as bundles × query-expansion runs (~4-5) × search calls (~4):

1. **One tokio runtime + cached LanceDB connections.** `lance.rs` builds a fresh `tokio::runtime::Builder::new_current_thread()` + `lancedb::connect()` in 8 functions (lines 189, 327, 394, 661, 830, 955, 1295, 1549). Store a shared runtime in `McpContext` (or `OnceLock`) and cache open `Connection`/`Table` handles keyed by directory. Dozens of runtime spin-ups + reconnects per query → one.
2. **Enumerate bundles once per request.** `store.rs:491 list_all_bundles` re-scans the store dir and re-parses every `manifest.json` from disk; a single `query` triggers it 10+ times (via `collect_query_hits` per run at `mcp.rs:1845`, scope validation at `mcp.rs:1957`, and per-hit via `resolve_bundle_spec` at `mcp.rs:1611,2816`). Enumerate once at the start of `tool_query` (or cache in `McpContext` with an mtime check) and thread the `Vec<BundleInfo>` through.
3. **Reuse reranker context across a batch.** `reranker.rs:440 score_pair` creates a new `LlamaContext` per (query, doc) pair; `tool_query` calls it for every pool candidate (pass 1) and every KWIC window of every promoted hit (pass 2) (`mcp.rs:2326,2374`). Add a batch `score_pairs(query, &[doc])` reusing one context with `clear_kv_cache()` between decodes — the embedder already does exactly this (`embedding.rs:460 embed_documents_batch`).
4. **Batch symbol-source lookups.** `fmt_codegraph_with_source` (`mcp.rs:2834`) does one LanceDB round-trip per node for `get_callers`/`get_callees` (up to `limit` round-trips) — fixable via #1 plus a single scan with an IN-filter.
5. **Minor:** read `SEMPKG_DEBUG`/`SEMPKG_NO_COMBINE` env vars once at startup instead of per-query in 3 places.

## WS3 — Registry correctness, security, tests (HIGH)

1. **Fix the DOA Docker image.** `src/sempkg_registry/Dockerfile:8` sets `SEMBUNDLE_REGISTRY_ADMIN_PASSWORD=changeme` but `cli.py:12` reads `sempkg_registry_ADMIN_PASSWORD` — the container exits at startup. Standardize on `SEMPKG_REGISTRY_ADMIN_PASSWORD` (all-caps) across `cli.py`, Dockerfile, docker-compose, tests; do NOT bake a default password. Also fix the sibling casing bug in sembundle: env vars `SemBundle_REGISTRY_URL`/`SemBundle_TOKEN` (`sembundle/src/main.rs:105,110`, `publish.rs:18,20`) → `SEMBUNDLE_*`.
2. **Dockerfile installs unpinned deps by hand** (`pip install fastapi uvicorn...`, missing `cryptography` from `pyproject.toml`) → `COPY` project + `pip install .` so `pyproject.toml` is the single source of truth.
3. **Restore HTTP-layer tests.** `tests/test_registry_app.py` was deleted in commit `69ce42e` (stale `.pyc` remains); all 219 lines of `app.py` are untested. Rewrite with `TestClient` covering: publish happy path, bad/missing bearer (401), oversized upload (413), duplicate version (409), path-traversal filenames, missing manifest, admin token CRUD, signature download. Delete the stale `.pyc`.
4. **Atomic writes + concurrency.** `storage.py` writes `index.json` and bundles non-atomically with no lock; concurrent publishes can corrupt the index (`app.py:169` full `rebuild_index()` per publish is also O(all packages)). Use temp-file + `os.replace()`, serialize/lock publishes, update the index incrementally.
5. **Decompression-bomb guard.** `app.py:145` buffers up to 500 MB in memory, then `_extract_manifest` (`app.py:42`) does an uncapped `fh.read()` on a tar member. Stream upload to a temp file; cap the manifest read (~1 MB); reject absolute/`..` tar member paths.
6. **Auth via `Depends`.** Bearer parsing is hand-rolled in `_require_admin` (`app.py:23`) and inline in `publish_bundle` (`app.py:138-142`). Extract `require_admin`/`require_publish_token` FastAPI dependencies; inject storage/token_store via `app.state`. Also: `TokenStore` re-reads `tokens.json` on every request including inside async handlers (blocking the event loop) — cache with invalidation or run in threadpool.

## WS4 — CI, docs, repo hygiene (MEDIUM)

1. **Add a lint CI job**: `cargo fmt --all --check` + `cargo clippy --all-targets --all-features -- -D warnings` (CLAUDE.md mandates these but `.github/workflows/tests.yml` only runs `cargo test`). Optionally `ruff check` for both Python projects.
2. **CI never tests `sempkg_agent`**: root pytest `testpaths=["tests"]` skips `src/sempkg_agent/tests/`. Add a job installing `src/sempkg_agent[dev]` and running its `pytest -m 'not functional'`.
3. **Document the fourth app.** CLAUDE.md says "Three applications" but `src/sempkg_agent` (LangGraph agent server, 30 tracked files, own Dockerfile/compose/tests) is committed and undocumented. Update CLAUDE.md Code Structure + Common Commands.
4. **Working-tree cleanup**: delete stray `test.txt` (empty, root); gitignore `deploy/workspace/` and `.claude/` (a full stale worktree copy lives under `.claude/worktrees/embedding-name/` polluting searches); decide commit-vs-ignore for `.mcp.json`, `CLAUDE.md`, `scripts/mcp_query_probe.py` (look intentional — commit).
5. **Docs**: move the five `docs/plan-*.md` files (mixed tracked/untracked) into `docs/roadmap/` or `docs/plans/` to separate planning docs from reference docs.

## WS5 — God-file splits + testability (MEDIUM, structural; do after WS1/WS2)

1. **Split `sempkg/src/mcp.rs` (3,291 lines)** into `mcp/{jsonrpc,schema,format,query,context}.rs` — transport loop, tool schemas, formatters, the `UnifiedHit` query pipeline (RRF/dedup/KWIC/merge), and `McpContext` + simple tool methods respectively.
2. **Split `sempkg/src/main.rs` (2,535 lines)** into `commands/{add,sync,query,...}.rs`. Dedupe first: `BuildOptions` construction is copy-pasted between `add_from_github` (`main.rs:1662-1716`) and `add_from_local` (`main.rs:2298-2348`) with an identical `resolve_dirs` closure repeated 6×; extract `resolve_dirs()` + a single `build_and_install_bundle()`.
3. **Split `sembundle/src/build.rs` (1,498 lines)** into `build/{codegraph,lance_docs,code_index,chunk,tools}.rs`. While there: remove the dead `_cwd` param and never-used `passthrough=false` branch in `invoke` (`build.rs:1383,1407-1417`) — and note CodeGraph failures currently capture no stderr; fix that. Factor the near-identical chunk-emission tails of the two symbol extractors (`:703-734` vs `:900-929`).
4. **Add `src/lib.rs` to sempkg** (currently bin-only) re-exporting modules, making `main.rs` a thin wrapper — unlocks `tests/` integration tests for the currently untested `store.rs`, tool dispatch, and install/sync orchestration.
5. **Error-handling consistency**: delete 7 never-constructed `SempkgError` variants (`error.rs`: `ManifestNotFound`, `PackageNotFound`, `InvalidBundle`, `NotIndexed`, `RerankerModelNotFound`, `Db`, `Reranker`); pick one convention per layer (typed only where callers match variants, else `anyhow` + `.context()`). In sembundle, add `PackError::Lance/Db` variants — `InvalidField` is currently abused for LanceDB/DB failures (`build.rs:400,410,419,591,608,625,1070,1105,1121,1131`), producing misleading "invalid field" errors. Consider consolidating the per-subcommand error enums (they're erased to `Box<dyn Error>` in `main.rs` anyway).

## WS6 — Mechanical quick wins (LOW effort, safe, can go first or fold into other WSs)

- `#[derive(Default)]` on `DependencyEntry` (13 fields, built field-by-field in ~7 places; `main.rs:216,242,269`) — collapses ~120 lines.
- One `CodegraphNode` struct + `parse_codegraph_array()` — the `{node, score}` JSON envelope is parsed independently in ≥4 places (`reranker.rs:580`, `mcp.rs:327,1787,2607,2667`).
- Merge near-duplicate `apply_rerank_to_codegraph_json`/`apply_rerank_to_codegraph` (`mcp.rs:2593,2660`) via a render callback.
- Shared `loc_key(path, start, end)` helper — the `"{path}:{s}-{e}"` idiom appears 12× across `mcp.rs`, `lance.rs`, `reranker.rs`.
- Add `Rerank::score_all()` to the trait — CLI `run_query` (`main.rs:960-980`) and `tool_query` pass-1 both hand-roll the trait's default rerank loop.
- Unify `resolve_codegraph_path`/`resolve_lance_path` (duplicated `main.rs:1965,2001` vs `mcp.rs:1259,1293`) into one resolver in `store.rs`.
- Fix silent misdirection: `dir.to_str().unwrap_or(".")` at 7 lance connect sites and `sembundle/build.rs:407,1067` writes indexes to CWD on non-UTF-8 paths — return an error instead.
- Dead code: `Embed::embed_document` (trait + all impls, never called), `UnifiedHit.best_window_first`, `cli_update`'s ignored `_collection_name` param; magic numbers `12_000` char budget (defined twice, `mcp.rs:59,2844`) and RRF `60.0` (inline twice) → named constants.
- Flatten shared `BundleIdentityArgs` across sembundle `Pack`/`Build` subcommands (~8 duplicated clap args).
- Streaming pack (optional, larger): `pack.rs` loads the entire bundle into memory before writing (`:49-53,128-179`); two-pass stream (hash pass → tar-write pass) bounds peak memory at O(largest file); parallelize per-file hashing with rayon while there.

## Verification

- **Rust**: `cargo fmt --all --check && cargo clippy --all-targets --all-features -- -D warnings && cargo test` from the new workspace root (WS1) or per-crate until then. After WS1, verify a bundle built by `sembundle` installs and signature-verifies via `sempkg` (round-trip test).
- **WS2**: benchmark an MCP `query` call before/after (e.g. via `scripts/mcp_query_probe.py` or `/run-tests` → `tests/test_mcp_functional.py`); functional suite must stay green (note: minutes-long, uses local reranker model).
- **WS3**: `.\.venv\Scripts\python.exe -m pytest tests/ -q`; build the registry Docker image and confirm `serve` boots with only `SEMPKG_REGISTRY_ADMIN_PASSWORD` set; new `test_registry_app.py` covers the listed endpoint cases.
- **WS4**: push a branch and confirm the new lint + agent-test CI jobs run and pass.
- **WS5/WS6**: pure refactors — full Rust test suite green, MCP functional suite green, no CLI output changes (CLAUDE.md requires output stability).

# ADR-006: Cutover — the native `semgraph` indexer is the only indexer

**Date:** 2026-07-09
**Status:** Accepted
**Extends:** [ADR-003](adr-003-semgraph-native-writer.md) (native writer +
multi-root paths), [ADR-004](adr-004-semgraph-resolution-and-sync.md) (resolution
+ sync), [ADR-005](adr-005-tier3-language-packs.md) (tier-3 language packs)

---

## Context

Issue #78 decouples sempkg from the `@colbymchenry/codegraph` Node CLI. Phase 1
delivered a native Rust *reader* for the schema-v4 `codegraph.db`; Phase 2
delivered the native *writer*, resolution, incremental `sync`, all 14 language
packs, and a parity harness that gates the native indexer against a pinned
CodeGraph 0.9.7 build (ADR-003/004/005; parity hardening in PR #90).

The read path was already switched to the native reader (`sempkg::graph` over
`semgraph::GraphDb`) — querying an installed bundle already needed no Node. What
remained were the two *write* sites and the surrounding tooling:

- `sembundle build` shelled out to `codegraph init --index` / `index --force`
  per source directory, then copied each `.codegraph/` into the bundle's
  `graph/`. Because CodeGraph writes a fixed `codegraph.db` filename, the
  per-directory copies overwrote each other — the root cause of **issue #79**
  (multiple `-s` roots silently dropped to the last one).
- `sempkg pkg add` / `pkg reindex` shelled out to `codegraph init`/`sync`.
- CI installed `npm install -g @colbymchenry/codegraph` (unpinned) in three
  workflow locations; a `codegraph` binary on `PATH` was a hard prerequisite for
  building bundles or indexing local packages.

This ADR records the **cutover**: making the native indexer the *only* indexer
and removing the CodeGraph/Node dependency from `sembundle`, `sempkg`, and CI
entirely.

## Decision

### 1. `sembundle build` indexes natively into one graph

`run_codegraph` is replaced by `run_semgraph`, which calls
`semgraph::index_roots(&roots, graph/codegraph.db, &opts)` over **all**
`--source-dir` roots at once, writing a single schema-v4 database. This
**completes the user-facing fix for issue #79**: multiple roots — including a
same-basename pair like `backend/api` + `frontend/api` — all land in one graph,
namespaced apart by `semgraph`'s stored-path scheme (ADR-003). The code-index
extractor (`extract_chunks_from_codegraph_db`) reverses that namespacing with
`semgraph::resolve_stored_path` to read each file from the correct root; the
stored chunk path stays equal to the graph node's `file_path`, preserving the
`read_symbol`/`read_code` join key.

The `.cmd`/`which` shim handling, `find_tool`/`invoke`/`copy_dir_into`, and the
`PackError::ToolNotFound`/`ToolFailed` variants are deleted. `sembundle` no
longer depends on the `which` crate.

### 2. `sempkg pkg add` / `pkg reindex` index natively

`codegraph::init_and_index`/`sync` now call `semgraph::index_roots`/`sync` into
`<project>/.codegraph/codegraph.db` (the same on-disk location as before, so
`db_path`/`is_indexed`/the reader are unchanged). The CLI wrapper
(`codegraph_exe`, `run`) and the `SempkgError::CodegraphNotFound`/`CodegraphError`
variants are deleted. **"CodeGraph not installed" is now unreachable** anywhere in
sempkg — building and querying both run in-process.

### 3. Version stamping

`manifest.codegraph_version` becomes `"sempkg-native/<semgraph crate version>"`
(e.g. `sempkg-native/0.6.0`), sourced from `semgraph::VERSION`. The spec (§4.2)
treats the field as free-form; nothing in `sembundle`, `sempkg`, or the registry
validates its format (confirmed across the codebase). `sembundle build`'s
`--codegraph-version` flag is now optional and defaults to this value; `sembundle
pack` (which packages a pre-built directory) keeps it required.

### 4. CI runs the native path

All `npm install -g @colbymchenry/codegraph` steps and the Node setup used only
for them are removed from `.github/workflows/tests.yml` (two jobs) and
`release.yml` (one job). The functional MCP job still builds real bundles — now
via the native indexer (`sempkg add colbymchenry/codegraph@v0.9.7 --source-dir …`
builds a bundle *from* the CodeGraph repo's source, it no longer installs the
CLI). The release workflow drops `--codegraph-version` and lets the native
default stamp the manifest.

### 5. Backward compatibility

The on-disk bundle format is **unchanged** — still schema v4, same layout. The
Phase-1 reader serves native-built and CodeGraph-0.9.7-built graphs
interchangeably, so bundles published before the cutover keep working. No
`spec_version` bump; the spec change is documentation-only (§6–§8 reworded to be
producer-neutral).

## Parity — the honest numbers

The native indexer is accepted against a pinned CodeGraph 0.9.7 baseline via the
parity harness (`docs/parity-harness.md`). On this repo's `src/` (the hardening
target, PR #90):

| Metric | Native vs CodeGraph 0.9.7 |
|---|---|
| **Nodes** | **≈99.9%** (`99.86%`; the residual is a Python nested-function convention) |
| **`calls` — honest recall** | **83.90%** (golden 2640; 1847 matched, 368 verified CodeGraph fabrications whitelisted, **793 declined-and-counted**) |
| **`calls` — true genuine-recall** | **≈82.6%** (point estimate; ~81–84% over the ambiguous split) |

The `calls` number is deliberately **not** inflated to the ≥90 harness bar. PR
#90 hand-classified a random sample of the 793 declined edges (n=63, ~8%):

- **~46%** are CodeGraph *fabrications* — it bare-name-resolves an external/std
  call (`str::find`, `HashMap::get`, `Digest::finalize`, clap `parse`) to an
  unrelated same-named local symbol. semgraph is correct to decline these.
- **~44%** are *genuine recall loss* — CodeGraph resolved correctly and semgraph
  declined only because it won't guess a receiver type it can't infer (a local
  bound from a bare-fn return, a chained call, an untyped Python receiver).
- **~10%** ambiguous.

Extrapolated: of the 793, roughly **365 CodeGraph fabrications**, **352 genuine
losses**, **76 ambiguous**. So about half the apparent gap to 90 is CodeGraph
noise semgraph is right to drop; the other half is a real ~15%-of-golden recall
loss whose closure is a **precision-preserving receiver-inference follow-up**
(return-type-of-local, chained-call, typed-Python) — explicitly **not** a return
to name-based guessing. Full write-up and per-edge examples: **PR #90**.

We accept this tradeoff for cutover: `get_callers`/`get_impact` favor precision
(no fabricated edges) over recapturing CodeGraph's fabricated tail, and the
number stays honest rather than gamed to the threshold.

## Consequences

**Positive**

- Bundle producers and consumers need **zero external tooling** — no Node, no
  global npm package, no version skew between the CLI that built a DB and the one
  that queries it (the original #78 failure mode is gone).
- Issue #79 is fixed for real: multi-root builds are lossless.
- In-process indexing is faster (rayon-parallel parse, single-writer SQLite) and
  removes per-invocation Node process startup; CI drops a Node toolchain install.
- Windows friction (`.cmd` shims, `which` fallbacks) is deleted.

**Negative / accepted**

- `calls` recall trails CodeGraph 0.9.7's *reported* number by design; ~15% of
  golden calls are genuinely not emitted pending receiver inference (above).
- **Output-stability deviation:** `sempkg pkg add`/`reindex` previously printed
  CodeGraph's CLI stdout; they now print a one-line native summary (`Indexed N
  files, M nodes, K edges.`). The build log label changed from `codegraph:
  <ver>` to `indexer: sempkg-native/<ver>`. MCP tool response *shapes* and
  `read_symbol`/`read_code` coordinates are unchanged.

**Attribution.** `NOTICE` stays: the native indexer uses tree-sitter and grammar
queries ported from CodeGraph (MIT) and other MIT/Apache grammars, retained per
their licenses.

## Alternatives considered

- **Keep a `--indexer codegraph` escape hatch** (the soak flag from #78's plan).
  Rejected for the cutover: it would re-introduce the Node dependency and the
  version-skew failure mode the whole effort exists to remove, and the read path
  has already soaked on the native reader. The pinned CodeGraph baseline survives
  only in the parity harness (developer-only, never a runtime/CI dependency of
  the product).
- **Chase ≥90 `calls` parity before cutover** by re-adding name-based receiver
  resolution. Rejected: that is exactly the fabrication semgraph declines; it
  would trade precision for a number. Deferred to a precision-preserving
  receiver-inference follow-up.

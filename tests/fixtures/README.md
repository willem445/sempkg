# Graph reader test fixtures

This directory holds the **compatibility contract** for the native Rust CodeGraph
reader (issue #78, Phase 1). It pins down exactly what a schema-v4
`codegraph.db` produced by CodeGraph looks like, so the reader can be tested
against a real, committed artifact instead of a freshly-generated one (which
would vary by machine and CodeGraph version).

## Contents

| Path | What it is |
|------|------------|
| `graph-src/` | A small multi-language source tree (Rust + Python + TypeScript) that exercises the node/edge kinds the reader must support. |
| `codegraph-v4.db` | The SQLite graph database produced by indexing `graph-src/` with **CodeGraph 0.9.7** (schema version 4). Treat this file as a golden fixture — do not hand-edit it. |

## What the fixture exercises

`graph-src/` is deliberately tiny but covers every construct the reader cares
about, in each of the three tier-1 languages:

- **functions** — free/module-level functions (`hypot`, `circle_area`, `magnitude`, …)
- **methods** — instance/associated methods (`Point::new`, `Circle.area`, `Report.measure`, …)
- **classes / structs / enums + members** — `Circle`/`Report` (classes), `Point` (Rust struct with fields), `Shape`/`Kind` (enums with variants/members)
- **imports** — `use` (Rust), `from … import` (Python), `import { … }` (TypeScript)
- **cross-file calls** — every language's entry file calls into its sibling file (e.g. `rust/lib.rs` → `rust/geometry.rs`), producing resolved `calls` edges across `file_path` boundaries
- **type alias** — `Scalar` (Rust `type` alias and TypeScript `type` alias)
- **async fn** — `fetchAndMeasure` (TypeScript, flagged `is_async=1`); Rust `fetch_and_measure` and Python `gather_measurements` are also present as nodes
- **unresolvable references** — `python/unresolved.py` deliberately calls a name
  defined nowhere (`totally_undefined_symbol`) and imports a non-existent module
  (`this_module_does_not_exist`), to probe the `unresolved_refs` table (see the
  empty-tables note below)

### Observed contents of `codegraph-v4.db`

Indexing produced **7 files, 55 nodes, 116 edges**. As reported by CodeGraph 0.9.7:

- Node kinds: `class`, `enum`, `enum_member`, `file`, `function`, `import`, `method`, `struct`, `type_alias`, `variable`
- Edge kinds: `calls`, `contains`, `imports`, `instantiates`, `references`
- `schema_versions` max = **4**

> **Note on CodeGraph 0.9.7 quirks** (captured faithfully — the contract is
> "what the tool actually emits", not an idealized graph):
> - Only the **TypeScript** async function is flagged `is_async=1`; the Rust and
>   Python async definitions are recorded as ordinary `function` nodes.
> - Python's `Scalar = float` is recorded as a `variable`, not a `type_alias`
>   (only Rust and TypeScript produce `type_alias` nodes here).
> - The unresolvable call in `python/unresolved.py` produces **no `calls` edge**
>   — CodeGraph silently drops references it cannot resolve. The non-existent
>   import is still recorded as an `import` node/edge (imports are not resolved
>   against real modules).

### Empty tables: `unresolved_refs` and `project_metadata`

Two schema-v4 tables are present but **always contain 0 rows** in a CodeGraph
0.9.7-produced DB. This is not an artifact of the fixture — it is how 0.9.7
behaves for **any** input, verified both empirically and against the tool's
source:

- **`unresolved_refs` — always empty by design.** Indexing resolves references
  via `resolveAndPersistBatched` (the only path `init --index` / `index` uses).
  That routine deletes *both* successfully-resolved refs *and* the ones it fails
  to resolve from `unresolved_refs` after each batch (to avoid reprocessing), so
  the table is fully drained by the time indexing finishes. `python/unresolved.py`
  confirms this: even with a call to a nowhere-defined name and an import of a
  non-existent module, `unresolved_refs` stays at 0. The table is used only as
  transient scratch space during resolution.
- **`project_metadata` — never written.** CodeGraph 0.9.7 defines `setMetadata`
  in its DB layer but never calls it anywhere in the shipped code, so nothing is
  ever inserted. The `codegraph_version` recorded in a SemBundle's
  `manifest.json` comes from the `sembundle` build pipeline, **not** from this
  table.

**Implication for the Phase 1 reader:** do not depend on either table for graph
data. A reader may still `SELECT` from them (they exist), but must treat 0 rows
as the normal, expected case for 0.9.7-built bundles.

## How `codegraph-v4.db` was generated

Regenerate it exactly as follows (from the repository root):

```bash
# Tool version — must be exactly 0.9.7 (the pinned schema-v4 producer):
codegraph --version   # -> 0.9.7

# Index the fixture tree; CodeGraph writes .codegraph/codegraph.db under it:
codegraph init --index tests/fixtures/graph-src

# Promote the produced DB to the committed golden fixture, then discard the
# transient .codegraph/ working directory (not committed):
cp tests/fixtures/graph-src/.codegraph/codegraph.db tests/fixtures/codegraph-v4.db
rm -rf tests/fixtures/graph-src/.codegraph
```

If you ever regenerate this fixture with a newer CodeGraph, update the schema
version, counts, and quirk notes above — and be aware that doing so changes the
compatibility contract the reader is tested against.

## `parity-whitelist.json`

The parity harness (issue #78 Phase 2c; `docs/parity-harness.md`) diffs a
semgraph-built graph against `codegraph-v4.db`. `parity-whitelist.json` lists the
**known-better** deviations from CodeGraph 0.9.7 recorded in ADR-003/ADR-004
(`is_async` correctness, docstring cleanups, and the CodeGraph duplicate
return-type `references`), each with a justification. Whitelisted diffs are
reported separately and do not count as parity failures. The offline gate
`src/semgraph/tests/parity_offline.rs` exercises the golden DB against this
whitelist and requires ≥95% node / ≥90% `calls` parity.

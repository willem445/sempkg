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

### Observed contents of `codegraph-v4.db`

Indexing produced **6 files, 52 nodes, 113 edges**. As reported by CodeGraph 0.9.7:

- Node kinds: `class`, `enum`, `enum_member`, `file`, `function`, `import`, `method`, `struct`, `type_alias`, `variable`
- Edge kinds: `calls`, `contains`, `imports`, `instantiates`, `references`
- `schema_versions` max = **4**

> **Note on CodeGraph 0.9.7 quirks** (captured faithfully — the contract is
> "what the tool actually emits", not an idealized graph):
> - Only the **TypeScript** async function is flagged `is_async=1`; the Rust and
>   Python async definitions are recorded as ordinary `function` nodes.
> - Python's `Scalar = float` is recorded as a `variable`, not a `type_alias`
>   (only Rust and TypeScript produce `type_alias` nodes here).

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

# Parity harness (`semgraph`-vs-CodeGraph-0.9.7)

The parity harness quantifies how closely the native **semgraph** indexer
(issue #78, Phase 2) reproduces the graph a pinned **CodeGraph 0.9.7** build
produces for the same source tree. It exists so language packs can be accepted
against **objective thresholds** instead of eyeballing diffs, and so CI can gate
the eventual cutover from the CodeGraph CLI to semgraph.

- **Tool:** `cargo run -p semgraph --bin parity -- <tree> [flags]`
- **Core library:** `semgraph::parity` (`src/semgraph/src/parity.rs`) — the pure,
  unit-tested diff engine.
- **Offline CI gate:** `src/semgraph/tests/parity_offline.rs` (runs under
  `cargo test --manifest-path src/semgraph/Cargo.toml`, no Node/CodeGraph
  needed).
- **Whitelist:** `tests/fixtures/parity-whitelist.json` (known-better deltas
  from ADR-003/ADR-004).

## What it measures

Both graphs are read out of a schema-v4 `codegraph.db` and diffed:

| Entity | Match key | Notes |
|--------|-----------|-------|
| **Nodes** | `(kind, qualified_name, file_path)` | `--strict-line-range` additionally pins `(start_line, end_line)`. Otherwise the line range is compared as a whitelistable *attribute* so a one-line drift doesn't count as both a missing and an extra node. |
| **Edges** | `(source_qn, target_qn, kind)`, as a **multiset** | Preserves duplicate call sites and CodeGraph's duplicate return-type references. Edges are attributed to the **caller's** (source node's) language. |
| **Node attributes** | per matched node | `is_async`, `docstring`, `signature`, and (relaxed mode) line-range differences are reported and can be whitelisted. |

The report gives **per-kind** and **per-language** match percentages for nodes
and edges, plus missing/extra listings, plus a machine-readable JSON summary.
Match percentage is *recall* (golden items reproduced), crediting whitelisted
omissions.

### Acceptance thresholds

Two thresholds gate acceptance (the issue's Phase 2 criteria), both after the
whitelist is applied:

- `--min-nodes` (default **95**) — overall node match %.
- `--min-calls` (default **90**) — `calls`-edge match %.

The tool exits **0** when both pass, **1** when a threshold fails (CI gate), and
**2** on a harness error (bad DB, codegraph version mismatch, …).

## Running it

### Offline / CI mode (committed golden DB)

No Node or CodeGraph install required — compares against a prebuilt
`codegraph.db`:

```bash
cargo run -p semgraph --bin parity -- tests/fixtures/graph-src \
    --golden tests/fixtures/codegraph-v4.db \
    --whitelist tests/fixtures/parity-whitelist.json
```

The same comparison runs automatically in CI as
`src/semgraph/tests/parity_offline.rs` (via `cargo test -p semgraph`), which
asserts the fixture clears the thresholds (in fact 100% post-P2b) and that every
non-matching diff is accounted for by the whitelist.

### Live / dev mode (shell out to codegraph@0.9.7)

With `@colbymchenry/codegraph@0.9.7` installed
(`npm install -g @colbymchenry/codegraph@0.9.7`), omit `--golden` and the
harness builds the CodeGraph side itself:

```bash
cargo run -p semgraph --bin parity -- ./some/tree \
    --whitelist tests/fixtures/parity-whitelist.json
```

Live mode:

- verifies `codegraph --version` is exactly **0.9.7** and fails with an
  actionable message otherwise;
- runs `codegraph init --index <tree>` (or `index --force` if the tree already
  has a `.codegraph/` index), reading `<tree>/.codegraph/codegraph.db`;
- indexes a **single** root (CodeGraph has no single-DB multi-root mode); for
  multi-root comparisons build a DB yourself and pass `--golden`;
- removes the `.codegraph/` directory it created afterwards — unless the index
  pre-existed or you pass `--keep-codegraph`. A generated `codegraph.db` is
  large and machine-specific; **do not commit it** (report numbers only).

### Useful flags

| Flag | Effect |
|------|--------|
| `--min-nodes <pct>` / `--min-calls <pct>` | Acceptance thresholds (defaults 95 / 90). |
| `--whitelist <json>` | Apply a known-better-delta whitelist. Without it, every delta counts. |
| `--strict-line-range` | Pin `(start_line, end_line)` into the node match key. |
| `--json` | Print the JSON summary to stdout (the human report goes to stderr). |
| `--json-out <path>` | Write the JSON summary to a file. |
| `--keep-codegraph` | Live mode: keep the `.codegraph/` index this run creates. |

## The whitelist

`tests/fixtures/parity-whitelist.json` records **intentional, documented**
deviations where semgraph is *better than* CodeGraph 0.9.7 (ADR-003/ADR-004).
A whitelisted diff is still reported (tagged `[whitelisted]` with its
justification) but is **not** counted as a failure. Never whitelist a genuine
regression — only a documented improvement.

```jsonc
{
  "node_attrs": [
    { "attr": "is_async",  "qualified_name": "*", "file": "*", "justification": "…" },
    { "attr": "docstring", "qualified_name": "*", "file": "*", "justification": "…" }
  ],
  "edges": [
    { "side": "missing", "kind": "references", "source": "*", "target": "*",
      "only_duplicates": true, "justification": "…" }
  ],
  "nodes": []
}
```

- **`node_attrs[]`** — `attr` ∈ `{is_async, docstring, signature, line_range}`;
  optional `qualified_name`/`file` globs and an exact `language`.
- **`edges[]`** — `side` ∈ `{missing, extra}`; optional `kind`, `source`/`target`
  globs. `only_duplicates: true` whitelists a *missing* edge instance **only**
  when semgraph still emits that key at least once (a duplicate-multiplicity
  omission, not a true recall gap).
- **`nodes[]`** — `side` ∈ `{missing, extra}`; optional `kind`, `qualified_name`/
  `file` globs.
- Globs support `*` matching any run (`Array<*>`, `python/*`).

The three current entries correspond exactly to the three ADR-003/004
known-better categories: `is_async` correctness across all languages, docstring
cleanups, and the omission of CodeGraph's duplicate nested-generic return-type
`references`.

## Adding a language to the acceptance flow

When a teammate adds a tier-2/tier-3 language pack to `semgraph`, wire it into
parity acceptance as follows:

1. **Add a fixture source tree.** Extend `tests/fixtures/graph-src` with a
   subdirectory for the language (or add a new fixture tree), exercising the
   node kinds (functions, methods, classes, imports, …) and at least one
   cross-file call so `calls`/`references` edges are produced.
2. **Regenerate the golden DB.** With codegraph@0.9.7 installed, rebuild
   `tests/fixtures/codegraph-v4.db` from the fixture tree per
   `tests/fixtures/README.md` ("How `codegraph-v4.db` was generated") and commit
   the updated DB. This is the compatibility contract.
3. **Run the harness** in live mode against a real-world tree in that language
   and record the per-language node and `calls` percentages:
   ```bash
   cargo run -p semgraph --bin parity -- <real-tree-in-that-language> \
       --whitelist tests/fixtures/parity-whitelist.json --json-out parity.json
   ```
4. **Whitelist only documented, known-better deltas.** If the language has an
   intentional deviation (as Rust/Python/TS do for `is_async`/docstrings), add a
   whitelist entry **with a justification and an ADR reference**. Do not
   whitelist genuine gaps.
5. **Gate on the thresholds.** A language pack is accepted for cutover when it
   clears `--min-nodes 95 --min-calls 90` on its fixture (the offline test
   enforces this) and reports healthy numbers on real-world trees. Report the
   real-world numbers in the pack's PR.
6. The offline test (`parity_offline.rs`) automatically covers every language in
   the shared fixture, so no per-language test wiring is required — just keep the
   fixture and golden DB in sync.

## Interpreting real-world numbers

The committed fixture is deliberately exact (100% nodes / 100% calls post-P2b) —
it is the *contract*, not a representative repo. Running the harness against a
large real tree will surface genuine gaps (unsupported node/edge kinds,
name-resolution recall at scale). Those are the actionable signal the harness
exists to produce: a language stays behind the CodeGraph default until it clears
the thresholds on real code, not just on the fixture.

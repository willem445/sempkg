# ADR-004: Native reference resolution (edges) + incremental sync

**Date:** 2026-07-08
**Status:** Accepted
**Deciders:** sempkg maintainers
**Supersedes/extends:** [ADR-003](adr-003-semgraph-native-writer.md) (Phase 2a writer)

---

## Context

Issue #78 decouples sempkg from the `@colbymchenry/codegraph` Node CLI. ADR-003
delivered the Phase 2a native **writer**: definition nodes + structural
`contains` edges, byte-compatible with a CodeGraph-built schema-v4
`codegraph.db`. It explicitly deferred the hard part — turning a `foo()` call
site into a resolved edge to the right node across files — to **Phase 2b**, and
left incremental re-indexing (`sync`) unbuilt.

This ADR records the Phase 2b decisions: how references resolve into
`calls`/`references`/`imports`/`instantiates` edges, what we do about the
`unresolved_refs` table, and how incremental sync stays canonically equal to a
from-scratch index.

The compatibility contract remains `tests/fixtures/codegraph-v4.db` (CodeGraph
0.9.7 over `tests/fixtures/graph-src`): **116 edges** = 51 `contains` + 15
`calls` + 41 `references` + 5 `imports` + 4 `instantiates`.

## Decision

### Two-pass, deterministic resolution (`semgraph::resolve`)

- **Pass 1** (parallel, per file, in `parse.rs`): alongside the definition nodes
  and `contains` edges, extract a list of **reference sites** — call / method-call
  / constructor / type-reference / import occurrences — each carrying its
  call-site coordinate (`edges.line`/`col` = the start of the call/`new`
  expression, the type identifier, or the import statement) and whatever *local*
  context the parser could infer (a qualified `Type::method` path, or a
  receiver's type inferred from local `let v = Ctor(..)` / `new T(..)`
  assignments and parameter type annotations). Sites are captured with small
  per-language `.scm` queries (`*.refs.scm`, one capture: call/`new` sites) plus
  a Rust-side walk of each definition's signature for type references.
- **Pass 2** (serial): build one global symbol table over *all* definitions and
  resolve each site to zero or one target node. Resolution is a pure function of
  the symbol table and the site's own file — **never** of file or thread order —
  so the same tree yields the same edges every run. Ambiguity is broken by a
  fixed precedence, then lexicographically by `file_path`.
- **Precedence** (matching CodeGraph 0.9.7's name-based heuristics): for a bare
  name, `same-file → import-target → unique-global → same-directory`. The global
  fallbacks are **language-scoped** (a Rust call never resolves to a same-named
  TypeScript function). A name still ambiguous after all tiers is **dropped** —
  *precision over recall for `calls` edges*, per the issue directive. Qualified
  `A::b` calls resolve against `qualified_name` directly; method calls resolve
  only when the receiver's type was inferred (an un-inferrable receiver is
  dropped, not guessed). Constructors (`new T` / a bare call to a class name)
  emit `instantiates`; type identifiers in signatures emit `references`.
- **`edges.metadata`** carries `{"confidence":x,"resolvedBy":s}` matching the
  fixture's convention (`qualified-name`/`instance-method`/`import`/`exact-match`,
  with confidence tracking global name uniqueness).

### Parity outcome

- **`calls` edges: exact** — all 15 fixture `calls` reproduced on
  `(source_qn, target_qn, kind, line)`, including duplicate call sites
  (`total_distance → Point::new` twice). This is the issue's graded metric
  (acceptance ≥ 90%; we hold 100% on the fixture to leave margin on real repos).
- **`imports` (5) and `instantiates` (4): exact.**
- **`references`: reproduced with a documented whitelist.** We emit each type
  occurrence once; CodeGraph 0.9.7 double-emits the return-type reference for
  five TS signatures whose return type is nested in a generic
  (`Array<…>`/`Promise<…>`). Those five duplicate second-copies are the only
  fixture references we do not reproduce — deterministic, whitelisted in
  `tests/resolve_parity.rs`. Python type annotations emit **no** `references`
  (CodeGraph emits none for Python; we match).

### `unresolved_refs`: match the observable empty behavior

CodeGraph 0.9.7 drains `unresolved_refs` after every batch, so a finished DB
always has **0 rows** for any input (documented in `tests/fixtures/README.md`
and ADR-003). **Decision: match that observable behavior** — a site we cannot
resolve is simply dropped, never persisted. Rationale:

- It keeps a sempkg-built DB byte-indistinguishable from a CodeGraph one, which
  is the whole point of the decoupling; the reader already treats 0 rows as
  normal, and the P2c parity harness sees the same empty table on both sides.
- Populating it "as the schema intends" (with a `candidates` JSON) would be a
  *visible divergence* from 0.9.7 that P2c would have to special-case, for no
  consumer benefit — nothing in sempkg reads `unresolved_refs`.

The (small) cost is that the durable record that *could* help re-resolve a
now-satisfiable reference on incremental sync is absent; see the sync boundary
below, which handles the practical cases without it.

### Incremental sync (`semgraph::sync`)

`sync(roots, db, opts)` re-parses only files whose `files.content_hash` (SHA-256,
from Phase 2a) changed, plus added/deleted files, and re-resolves the edges the
delta invalidates. The result is **canonically equal** to a fresh `index_roots`
(same nodes/edges/files, modulo the autoincrement `edges.id` and `*_at`
timestamps), proven by `tests/sync_tests.rs` for modify-callee / modify-caller /
add / delete / rename / no-op scenarios.

**Invalidation set (the correctness core):**

1. Delete the nodes of changed+deleted files. The `edges` FK `ON DELETE CASCADE`
   removes every incident edge — including resolved edges *from unchanged files
   into* a changed/deleted file's symbols (the "edges into its symbols" case the
   issue calls out). The writer DDL is byte-identical to 0.9.7, which *does*
   declare these cascades, so we rely on them (with `PRAGMA foreign_keys=ON`)
   rather than hand-deleting edges.
2. Re-resolve the reverse-dependency blast radius: `changed ∪ added ∪ affected`,
   where `affected` = unchanged files holding a resolved edge whose **target
   file** is in the delta **or** whose **target name** is in `delta_names` (the
   set of symbol names whose global multiplicity changed — names removed from
   gone files ∪ names introduced by the fresh parse). The `target-name` clause
   is essential: renaming/removing a symbol elsewhere can flip a name from
   unique→ambiguous and thus change an *unrelated* file's edge (target and even
   confidence), which a target-file-only rule would miss. Only these files are
   re-parsed (for their sites); their nodes/`contains` edges are untouched.
3. Node ids hash `kind`+`qualified_name`+`file_path`, so a rename/move changes a
   symbol's id; re-resolving the affected sources against the rebuilt table
   re-points them exactly as a from-scratch index would.

**Documented boundary:** because `unresolved_refs` is not persisted (above), a
purely *additive* change that newly satisfies a **previously-dropped** reference
in an unchanged file with no prior edge into the delta is not re-resolved
incrementally (that file is not in `affected`). Dependency-directed edits
(modify/add/delete/rename a file others call into) are always exact; callers who
need exactness after such an additive change can re-run `index_roots`.

### FTS / integrity

Node deletes and inserts flow through the existing `nodes_ai`/`nodes_au`/
`nodes_ad` triggers, so `nodes_fts` stays in lockstep with `nodes` across a sync;
`tests/sync_tests.rs` asserts row-count parity and runs the FTS5
`integrity-check` plus `PRAGMA integrity_check` after each scenario.

### `IndexStats` reports persisted counts

`index_roots` now reads its `node_count`/`edge_count` back from the database
after writing, rather than from the raw in-memory vectors. The writer's
`INSERT OR IGNORE` dedups nodes that collapse to the same id
(`kind`+`qualified_name`+`file_path` — e.g. two `impl` blocks defining a
same-named method), so the persisted count can be lower than `nodes.len()`.
Reading it back keeps `index_roots` and `sync` reporting the *same* metric, which
is what makes "sync equals from-scratch" checkable.

## Consequences

- `sembundle build` / `sempkg index` can produce a fully-resolved graph
  (nodes + all edge kinds) with no external tooling, and keep it current
  incrementally — the read path (ADR-003 Phase 1) opens it unchanged.
- The one resolution behavior that is *known-better* than 0.9.7 (language-scoped
  global resolution — no cross-language `calls`) and the `references` duplicate
  omission are the only intentional deviations; both are pinned in tests and are
  the P2c parity harness's whitelist, alongside ADR-003's `is_async`/docstring
  improvements.
- Node-id collisions (distinct definitions sharing `kind`+`qualified_name`+
  `file_path`) are silently deduped by the writer — a Phase 2a id-scheme
  property, applied identically by `index_roots` and `sync`. Tightening the id
  scheme is out of scope here and tracked for a later phase.
- Cutover of `sembundle`/`sempkg` from the CodeGraph CLI to
  `semgraph::index_roots`/`sync` remains a separate task (no sempkg/sembundle
  behavior changes in this PR).

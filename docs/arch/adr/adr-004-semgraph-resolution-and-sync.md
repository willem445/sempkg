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
0.9.7 over `tests/fixtures/graph-src`). Phase 2b delivered **116 edges** = 51
`contains` + 15 `calls` + 41 `references` + 5 `imports` + 4 `instantiates`; the
tier-1 hardening addendum extended the fixture with trait/interface + inheritance
constructs, bringing it to **135 edges** (adding 3 `extends` + 2 `implements`,
plus the `contains`/`references` of the new definitions).

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
  name, `same-file → import-target → unique-global → same-directory`. A name
  still ambiguous after all tiers is **dropped** — *precision over recall for
  `calls` edges*, per the issue directive. Qualified `A::b` calls resolve against
  `qualified_name` directly; method calls resolve only when the receiver's type
  was inferred (an un-inferrable receiver is dropped, not guessed). Constructors
  (`new T` / a bare call to a class name) emit `instantiates`; type identifiers
  in signatures emit `references`.
- **No cross-language resolution.** *Every* resolution path — the bare-name
  global fallbacks **and** the `qualified_name` (qualified/method-call) lookups —
  is **language-scoped** to the caller's file: a Rust `Point::dist()` never
  resolves to a same-named TypeScript `Point::dist`; if the caller's language has
  no matching definition the site is dropped, not pointed at a foreign symbol.
  The golden fixture has no cross-language name collision, so this is pinned by a
  purpose-built regression test in `resolve.rs` rather than by the fixture.
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

## Addendum — Tier-1 hardening (issue #78, real-repo parity)

Phase 2c's parity harness, run in live mode against this repo's `src/`, surfaced
the genuine gaps between semgraph and CodeGraph 0.9.7 on real code. Closing them
to clear the ≥95% node / ≥90% `calls` acceptance thresholds required the
following, all additive to the model above:

- **Inheritance edges.** semgraph now extracts `trait`/`interface` **nodes**
  (Rust `trait_item`, TS `interface_declaration`) and the `extends`/`implements`
  **edges** CodeGraph records: Rust supertrait bounds and `impl Trait for Type`,
  Python `class C(Base)`, TS `class … extends/implements`. Endpoints resolve
  through the same language-scoped name resolver. We deliberately do **not** emit
  TS `interface X extends Y` edges — CodeGraph 0.9.7's clause walker misses
  `extends_type_clause`, so emitting them would be a divergence, not parity.

- **Position-folded node ids (supersedes the "id-collision dedup" note above).**
  Non-`file` node ids now hash `(qualified_name, file_path, start_line,
  start_column)` instead of `(qualified_name, file_path)`. CodeGraph emits *one
  node per physical definition*; two definitions that collide on
  `(kind, qn, file)` — a `#[cfg]`-gated pair, several `impl` blocks each defining
  a same-named method, or repeated `use crate::…;` roots — are therefore all
  persisted rather than collapsed by the writer's `INSERT OR IGNORE`. This is the
  id-scheme tightening ADR-004 previously deferred, and it is what lifts real-repo
  `import`/`method` node recall to ~100%. `sync`-equals-from-scratch still holds
  (both use the same scheme).

- **Import extraction fidelity.** Imports are named by their module *root* and
  never qualified by an enclosing scope (`use std::…;` inside `mod tests` is
  `std`, not `tests::std`); `pub use` roots correctly (not `pub`); wildcard
  (`use x::*;`) and aliased top-level (`use x as y;`) forms and function-body-local
  imports are skipped — each matching CodeGraph 0.9.7 empirically.

- **Higher `calls` recall without sacrificing precision.** Two precision-safe
  additions: qualified calls may target an `enum_member` (tuple-variant
  constructors, `LanceError::MissingFile(p)`), and `self.field.method()`
  receivers are typed from the enclosing type's declared struct/class fields so
  `self.graph.callees()` resolves through `graph: GraphDb`. Crucially, a method
  call whose receiver type could **not** be inferred is **dropped**, never
  resolved by method name alone: a bare name (`.as_str()`, `.get()`, `.contains()`)
  routinely collides with a std/library method, so name-based resolution would
  fabricate an edge to a same-named project symbol. This is the deliberate
  precision-over-recall stance — semgraph emits **zero** fabricated calls.

- **CodeGraph `calls` fabrications are whitelisted, not imitated.** Because
  semgraph declines name-only method resolution, it does not reproduce the large
  fabricated component of CodeGraph 0.9.7's `calls`: cross-language resolutions (a
  Rust `stmt.execute()` pointed at a Python method) and external-library
  over-resolutions (every `.contains()`/`.ok()`/`.as_str()`/`.context()` resolved
  by bare name to the sole local `contains`/`ok`/… — often an associated
  constructor the code only ever calls in qualified form). The parity harness
  credits these as known-better via a `cross_language` edge rule plus **verified**
  external-library target entries, each with a count (`docs/parity-harness.md`).
  These are demonstrable fabrications, never genuine recall gaps. On this repo's
  `src/`, after this whitelisting the honest same-language `calls` recall is in the
  low-to-mid 80s%; the residual gap is CodeGraph resolving method calls on
  un-inferrable receivers (field-of-external-type, chained, return-typed) that
  semgraph precisely declines — the intended tradeoff, **not** whitelisted, so the
  number stays honest rather than inflated to the ≥90 bar.

## Addendum — evidence-based receiver-type inference (calls recall)

A residual-sample analysis of the declined method calls showed roughly half were
CodeGraph fabrications (correctly declined) and half were *genuine* — real calls
whose receiver type semgraph simply didn't infer. To close that genuine half
**without** relaxing precision, receiver typing is now resolved in two stages:

- **Pass 1** (`crate::parse`) describes each method-call receiver as a
  [`crate::resolve::RecvExpr`]: a concrete `Type` known syntactically (a typed
  parameter/local, `self`, a `self.field`, a constructor value), or — new — the
  **return value of a call** (`Return`/`Element`), captured as a
  [`crate::resolve::CallRef`] (bare `f()`, qualified `T::assoc()`, or a recursive
  method call for chains). An un-inferrable receiver carries *no* `RecvExpr` and is
  dropped, exactly as before.
- **Pass 2** (`crate::resolve`) types a `Return` receiver from the callee's
  resolved *return type* (parsed from its stored `signature`, with transparent
  wrappers `Result`/`Option`/`Promise`/`Optional`/… peeled and Rust `Self`
  resolved to the callee's enclosing type), and an `Element` receiver (a `for x in
  coll()` loop variable) from that return type's collection element. Every step
  reuses the same language-scoped, ambiguity-dropping call resolution, so an
  ambiguous or un-resolvable callee yields no type — precision stands.

This covers three forms from the sample: **return-type-of-local**
(`let db = open()?; db.query()`), **chained calls**
(`SymbolTable::build(&n).resolve_all()`), and **typed-Python receivers**
(parameter/variable annotations and constructor assignments), plus **for-loop
variables** iterating a typed collection. Inference is strictly evidence-based
(a resolved return type or annotation), never name-frequency; semgraph still emits
**zero** fabricated calls.

A return type that is itself a **collection** (a postfix array `T[]`, a slice, or
a `Vec`/`Array`/`List`… generic) or a **union** (`A | B`) types *nothing* on the
`Return` path: the value is an array/union, so `.method()` on it is a collection
method, not the element's — and a `T[]` receiver must not be read as `T` (only the
for-loop `Element` path reads the element type). Likewise the qualified/chained
callee path **drops** on same-language ambiguity rather than tie-breaking (two
same-named methods can return different types). These guards keep the "zero
fabricated calls" invariant total across every path (rev-44).

Measured on this repo's `src/` (live, convention-normalized): `calls` **83.2% →
86.4%**, and the true *genuine*-calls recall (fabrications excluded from the
denominator) **~82.6% → ~86.8%**. The remaining genuine gap is receivers with no
static evidence — untyped Python fixture parameters and iteration over a bare
local collection — which remain dropped rather than guessed.

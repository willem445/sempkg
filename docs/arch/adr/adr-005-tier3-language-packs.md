# ADR-005: Tier-3 language packs (Ruby, PHP, Kotlin, Swift, Scala, C#)

**Date:** 2026-07-08
**Status:** Accepted
**Extends:** [ADR-003](adr-003-semgraph-native-writer.md) (Phase 2a writer),
[ADR-004](adr-004-semgraph-resolution-and-sync.md) (Phase 2b resolution)

---

## Context

Issue #78 Phase 2 replaces the `@colbymchenry/codegraph` Node CLI with a native
Rust indexer. Phase 2a/2b delivered the writer, resolver and sync for the tier-1
languages (Rust, Python, TypeScript/JavaScript), whose definition extraction is a
flat tree-sitter query plus per-language `match` arms in `semgraph::parse`.

Phase 2c adds more languages. This ADR covers **part 3 — the tier-3 packs**:
Ruby, PHP, Kotlin, Swift, Scala, and C#. All six are supported by CodeGraph
0.9.7 (each has a dedicated extractor in the tool), so each has a golden
`codegraph.db` to hold parity against — there are **no "no-0.9.7-baseline"
languages** in this set.

## Decision

### A shared config-driven recursive-descent extractor (`semgraph::tier3`)

The tier-1 query-plus-`match` path does not model several things these six
languages need: JVM **package → `namespace`** scoping (Kotlin), Ruby **`module`**
nesting, the extra node kinds **`trait`/`interface`/`property`/`field`/
`constant`/`namespace`/`module`**, and **receiver-typed extension methods**
(Kotlin `fun T.m()`). CodeGraph itself handles every language with a *single*
recursive `TreeSitterExtractor` walker driven by per-language config objects, and
that structure is what makes byte-parity reachable.

So tier-3 mirrors it: one recursive walker (`tier3::Extractor`) driven by a
per-language `LangSpec` (node-type sets + small hook functions), with each
language a self-contained module (`tier3::{ruby, php, kotlin, swift, scala,
csharp}`). The walker produces the *same* `model` records and `resolve` reference
sites the tier-1 path does, so the Phase-2b resolver, the writer, and incremental
sync all work unchanged. `parse::extract` dispatches to it for tier-3 languages
(`Language::is_tier3`); the tier-1 path is untouched.

Two dispatch contexts match CodeGraph exactly: a **definition** traversal (file
root, class/interface/struct/enum bodies) that recognises definitions, imports,
and calls; and a **body** traversal (inside a callable) that recognises calls,
instantiations, bare calls, and nested definitions. The distinction is why a Ruby
`foo(...)` at statement level is shadowed by the import dispatch (Ruby imports are
`call` nodes) and emits no call, while the same call inside a method body does —
reproducing 0.9.7's observable behaviour.

Per-language `src/queries/<lang>.scm` + `<lang>.refs.scm` files are the reviewed
manifest of the definition / call node types. They are compiled against their
grammar in a unit test (`tier3::tests::manifests_compile_against_grammars`) so a
grammar-crate upgrade that renames a node type fails CI loudly.

### Grammar crates (workspace-pinned)

`tree-sitter-ruby` 0.23, `tree-sitter-php` 0.24, `tree-sitter-kotlin-ng` 1.1,
`tree-sitter-swift` 0.7, `tree-sitter-scala` 0.24, `tree-sitter-c-sharp` 0.23 —
all ABI-compatible with the workspace `tree-sitter` 0.25. (The older fwcd
`tree-sitter-kotlin` 0.3 pins `tree-sitter` 0.20 and is ABI-incompatible;
`tree-sitter-kotlin-ng` exposes the same node-type vocabulary CodeGraph's grammar
uses.)

### Per-language conventions (verified empirically against 0.9.7)

- **Qualified names** join emitted non-file ancestor names with `::`. Kotlin
  additionally wraps the file in a `namespace` node named after the `package`, so
  every top-level symbol is `com.pkg::Name`; a Kotlin extension method's name is
  overridden to `Receiver::method`. PHP/C#/Scala `namespace`/`package` clauses do
  **not** scope names (CodeGraph descends without a scope node).
- **Node kinds**: Ruby → `module`/`class`/`method`/`function`/`import`; PHP →
  `+trait`/`interface`/`enum`/`enum_member`/`field`/`constant`; Kotlin →
  `+namespace`/`type_alias`; Swift → `+struct`; Scala → `+trait`/`constant`;
  C# → `+struct`/`interface`/`property`/`field`.
- **Signatures**: only **Scala** function/method signatures are populated
  (`(params): Return`); Ruby/PHP/Kotlin/Swift/C# are NULL (CodeGraph defines no
  `getSignature`, or reads a node that is not a field). `field`/`property`/
  `constant` carry `Type name`-style signatures; imports carry the full statement.
- **Docstrings** reproduce 0.9.7's `getPrecedingDocstring` cleaning **including
  its quirks** — a `///` line keeps a stray leading `/` (Swift/C#), Ruby `#`
  comments keep the `#` — so docstrings match byte-for-byte.
- **Dropped** (matching 0.9.7): Ruby top-level `CONST = …`; Kotlin/Swift
  top-level properties and struct stored properties; C# locals; Swift protocol
  method requirements.

### Parity outcome and whitelist

Each language reproduces its golden **exactly** on the graded metrics — **100% of
nodes** (the `(kind, qualified_name, file_path)` keyset) and **100% of each edge
family we emit** (`calls`, `instantiates`, `imports`), graded as bidirectional
`(source_qn, target_qn, line, col)` multisets (no missing, no spurious) —
comfortably above the issue's ≥95%/≥90% gate. Pinned in `tests/tier3_parity.rs`,
which grades all three edge kinds per language.

Whitelisted deltas (the only ones), both disclosed here:

- **Synthesized interface→impl `calls`** (Kotlin/Scala/C# goldens): CodeGraph's
  Phase-5.5 heuristic emits `Interface::m → Impl::m`,
  `metadata.synthesizedBy = "interface-impl"`, with a **NULL call-site column**. A
  name-based implementation-bridging heuristic, not a real call site; replicating
  it is out of scope for the indexer. Excluded from the graded `calls` multiset by
  its NULL column.
- **Kotlin `import` target**: CodeGraph resolves `import a.b.C` to the imported
  *class* node; we point the `imports` edge at our own `import` node. One
  whitelisted missing+spurious pair (Kotlin only); every other `imports` edge
  across all six languages matches exactly.

**Scala emits no `instantiates`** — CodeGraph 0.9.7 has no Scala instantiation
handling, so `new Circle(...)` produces no edge. The Scala fixture includes a
`new Circle` construction and the parity test grades `instantiates` as an exact
**empty** multiset, proving the pack likewise emits none (no spurious edge).

Edge kinds **not** graded by the issue and not emitted (precision-first, and
consistent with tier-1's `references` handling): `implements`/`extends`
(inheritance) and `references` (type annotations). CodeGraph emits some of these
per language; a follow-up will align all languages with tier-1's edge families
once that hardening lands. This does not affect the node or edge acceptance
metrics.

### Bounded walk

The recursive walker is depth-capped (`MAX_DEPTH = 512`, a generous backstop):
pathologically/adversarially deep input (thousands of nested blocks) that would
otherwise overflow a rayon worker's stack — an uncatchable abort of the whole
`index_roots` — instead skips the deeper subtree and records the cap hit in the
file's `errors` column. All recursion (including the Ruby `module` and Scala
`extension` hooks) routes through the guarded `descend`. Pinned by
`deeply_nested_input_is_bounded_not_overflow`.

## Consequences

- `sembundle build` / `sempkg index` can natively index six more languages with
  no external tooling, byte-compatible with a CodeGraph-built schema-v4 DB; the
  Phase-1 reader opens the result unchanged.
- The tier-1 extraction path and its tests are untouched; tier-3 is additive
  (new `Language` variants, a new `tier3` module, new fixtures/goldens/tests).
- Inheritance (`implements`/`extends`) and type-reference (`references`) edges for
  tier-3, and CodeGraph's synthesized interface-impl calls, are deliberately out
  of scope and tracked for a later phase.

## Addendum — Edge-family alignment (issue #78)

The "later phase" above landed: all tier-2 and tier-3 packs now emit the
`extends`/`implements` (inheritance) and `references` (type-reference) edge
families, matching tier-1's edge families across all 14 languages. Verified
empirically against codegraph@0.9.7 and graded bidirectionally
(missing + spurious) in `tier2_parity.rs` / `tier3_parity.rs`.

- **Shared machinery.** Tier-3 gains two `LangSpec` hooks — `inheritance`
  (class-like nodes) and `type_refs` (callables) — feeding `SitePayload::Inherit`
  and `TypeRef`, plus a new `SitePayload::InheritAuto` whose edge kind is decided
  by the *resolved target's* kind (a target `interface`/`trait` → `implements`,
  else `extends`). Tier-2 reuses the tier-1 `parse::collect_inheritance_sites`
  path. Resolution and the writer are unchanged.
- **Per-language conventions** (all verified against 0.9.7): C — none; C++ —
  `extends` for every `base_class_clause` type (no `implements`); Go — struct /
  interface embedding → `extends`; Java — `extends`/`implements`/interface-extends;
  Ruby — superclass → `extends` (mixins none); PHP — `extends` + `implements`
  (incl. `use Trait`, anchored at the `use` statement); Kotlin — constructor-
  invocation delegate → `extends`, bare delegate → `implements`; Swift —
  target-kind decides (`InheritAuto`), `references` = **return type only**;
  Scala — only the primary parent after `extends` (`with` mixins dropped), never
  `implements`; C# — target-kind decides, `references` = return + parameter types.
- **`references`** are emitted by Go, Java, Swift, and C# (the languages 0.9.7
  emits them for); the rest emit none.
- **Whitelisted deviations** (precision-first, never imitating a fabrication):
  C++'s spurious `class → <method-return-type>` `extends` (0.9.7 misreads an
  in-class method return type as a base) is dropped and whitelisted; the Java
  type-name→constructor `references` misresolution is resolved to the class
  (known-better). CodeGraph's synthesized interface→impl `calls` (NULL column)
  remain excluded from graded `calls` everywhere.
- **Offline gate generalized.** `parity_offline.rs` now gates every committed
  language fixture (not just tier-1) on the ≥95%/≥90% thresholds.

(The `tier3::extract_enum` depth-guard carry-over noted in the original brief was
resolved independently by the tier-1 hardening work and is already in the base.)

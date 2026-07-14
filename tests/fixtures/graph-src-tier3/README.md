# Tier-3 language-pack fixtures

The **compatibility contract** for the native tier-3 language packs (issue #78
Phase 2c part 3): Ruby, PHP, Kotlin, Swift, Scala, C#. See
[`docs/arch/adr/adr-005-tier3-language-packs.md`](../../../docs/arch/adr/adr-005-tier3-language-packs.md).

## Contents

| Path | What it is |
|------|------------|
| `<lang>/` | A small source tree per language exercising the node/edge kinds that language produces (functions/methods, classes/structs/enums+members, traits/interfaces/modules, fields/properties/constants, imports, type aliases) plus at least one **cross-file call**. |
| `<lang>.db` | The schema-v4 `codegraph.db` produced by indexing `<lang>/` with **CodeGraph 0.9.7**. Golden fixture — do not hand-edit. |

Parity is asserted by `src/semgraph/tests/tier3_parity.rs`: a native
`index_roots` of each `<lang>/` must reproduce its golden's node keyset
(`kind, qualified_name, file_path`) and its `calls` / `instantiates` / `imports` /
`extends` / `implements` / `references` edge multisets (each graded
bidirectionally on `source_qn, target_qn, line, col` — no missing, no spurious).
The inheritance/type-reference families were added in the issue #78 edge-family
alignment; each fixture now exercises every kind CodeGraph 0.9.7 supports for its
language. All six currently reproduce their golden **exactly** (100% nodes and
100% of each graded edge family), above the ≥95%/≥90% gate.

## Whitelist (known 0.9.7 emissions not reproduced)

- **Synthesized interface→impl `calls`** (Kotlin/Scala/C#): CodeGraph's Phase-5.5
  heuristic emits `Interface::m → Impl::m` with `metadata.synthesizedBy =
  "interface-impl"` and a **NULL call-site column**. Not a real call site; the
  parity test excludes NULL-column `calls` for every language.
- **Kotlin `import` target**: CodeGraph resolves `import a.b.C` to the imported
  class node; we point the `imports` edge at our own import node. One whitelisted
  missing+spurious pair (Kotlin only); all other `imports` match exactly.
- **Scala `instantiates`**: 0.9.7 has no Scala instantiation handling, so
  `new Circle` yields no edge — graded as an exact empty multiset (the fixture's
  `new Circle` proves we emit none either).
The inheritance/type-reference families are reproduced **exactly** (no whitelist
needed): `extends` (all six), `implements` (PHP/Kotlin/Swift/C#), and `references`
(Swift return types; C# return + parameter types). Ruby `include` mixins, Scala
`with` mixins, and everything else CodeGraph does not emit are correctly absent.
See ADR-005's edge-alignment addendum.

## How the goldens were generated

From the repository root, with CodeGraph **0.9.7** (`codegraph --version`):

```bash
for lang in ruby php kotlin swift scala csharp; do
  ( cd tests/fixtures/graph-src-tier3/$lang \
    && codegraph init --index . \
    && cp .codegraph/codegraph.db ../$lang.db \
    && rm -rf .codegraph )
done
```

Regenerating with a newer CodeGraph changes the contract — update the goldens,
the parity expectations, and ADR-005 together.

# ADR-003: Native schema-v4 graph writer + multi-root path representation

**Date:** 2026-07-08
**Status:** Accepted
**Deciders:** sempkg maintainers

---

## Context

Issue #78 decouples sempkg from the `@colbymchenry/codegraph` Node CLI. Phase 1
delivered a native Rust *reader* for the schema-v4 `codegraph.db` (the
`semgraph` crate). Phase 2 replaces the CLI on the *build* side with a native
Rust indexer so `sembundle build` and `sempkg index` no longer shell out to
Node.

Two decisions needed recording before wiring the indexer into the build
pipeline (a later cutover task):

1. **What the writer emits**, so a sempkg-built graph is indistinguishable to
   the reader from a CodeGraph-built one (the bundle format must not change).
2. **How file paths are represented when indexing multiple source roots**, which
   is also the proper fix for issue #79 (multiple `-s`/`--source-dir` roots
   silently overwriting each other).

## Decision

### Native writer (`semgraph::writer`, `semgraph::index`)

- Produce a **byte-compatible schema-v4** database: the same tables, indexes,
  FTS5 contentless-external `nodes_fts` table with its `nodes_ai`/`nodes_au`/
  `nodes_ad` triggers, a `schema_versions` row declaring v4, and a final
  `ANALYZE`. The Phase 1 reader opens it unchanged (its tests still pass
  verbatim).
- **Node ids** keep CodeGraph's `"<kind>:<hash>"` shape: `file` nodes use the
  literal `file:<path>`; others hash `SHA-256(qualified_name \0 file_path)`
  truncated to 16 bytes. Nothing depends on the hash *content*, only on id
  equality within one DB, so our own hash is fine.
- **Parsing** uses the `tree-sitter` crate with per-grammar crates for tier-1
  languages (Rust, Python, TypeScript/JavaScript/TSX). Per-language `.scm`
  queries capture *definitions*; qualified names (`Outer::inner`), visibility,
  async/export flags, docstrings, and structural nesting are derived in Rust.
  The queries are adapted from CodeGraph's MIT-licensed tag queries (see
  `NOTICE`).
- **Phase 2a scope:** definition nodes + a `file` node per file + structural
  `contains` edges only. Call/reference/import **edge resolution is Phase 2b**;
  the per-file symbol output (qualified names + ids) is the symbol table a
  pass-2 resolver will consume.
- **Signatures** match CodeGraph byte-for-byte: the parameter list through the
  return type (no `def`/`fn`/name/generics, multi-line preserved), the full
  statement for imports, the assignment tail for variables, and NULL for
  types/members.
- **Deliberate improvements over 0.9.7** (the P2c parity harness must whitelist
  these — they are *known-better*, not regressions): `is_async` is set correctly
  for every language (0.9.7 flags only TS); docstrings are captured for Rust
  `///` and TS/JS comments (0.9.7 leaves Python NULL, keeps a stray leading `/`
  on Rust doc comments, bleeds a module `//!` header into the first definition,
  and misses `export`-wrapped TS declarations). We produce clean, complete
  docstrings instead. Both are documented in code (`parse.rs`) and pinned in
  tests.
- **Errored files** (non-UTF-8 / unreadable) are recorded with a `files` row
  whose `errors` column is populated, rather than silently dropped.
- **Performance:** files are parsed in parallel with rayon; a single-writer
  transaction batches all inserts. Indexing this repo's `src/` tree (78 files,
  ~1.8k symbols) takes ~0.27 s.
- **`files.content_hash`** is the SHA-256 of the file bytes — the anchor for
  Phase 2b incremental sync (unchanged files hash the same and can be skipped).

### File-path representation (issue #79)

`semgraph::index_roots` takes **multiple** roots and writes **one** database.
Stored `file_path`s must be unambiguous across roots yet stay consistent with
how consumers resolve a stored path back to disk (`sembundle`'s
`extract_chunks_from_codegraph_db` and `read_symbol`/`read_code` join a stored
path onto a source root).

- **Single root** → paths are relative to that root (`python/main.py`), exactly
  what CodeGraph emits. Existing single-root consumers are unaffected.
- **Multiple roots** → each root gets a **namespace** equal to the shortest
  trailing path suffix that distinguishes it from the other roots — usually the
  basename, extended by more components only when basenames collide
  (`-s backend/src -s frontend/src` → `backend/src` / `frontend/src`). A file's
  stored path is `"<namespace>/<relative>"`, which is globally unique.
- **Reverse mapping** for the cutover: `semgraph::resolve_stored_path(roots,
  stored)` re-derives the namespaces and returns `(root_index, relative)` by
  longest leading-component-prefix match; the consumer reads
  `roots[root_index].join(relative)`.
- **Overlapping/nested roots** (one root a filesystem ancestor of another) are
  rejected, since their namespaced paths could be ambiguous.

## Consequences

- Bundles built by sempkg-native and by CodeGraph are read by the same reader;
  the bundle format and `docs/sembundle-spec.md` are unchanged
  (`manifest.json.codegraph_version` remains a free-form string).
- #79 is fixed at the representation level: two roots (even same-basename ones)
  land in one DB with distinct paths, proven by regression tests.
- This PR adds the writer **library only**. Switching `sembundle`/`sempkg` from
  the CLI to `semgraph::index_roots` — and updating `extract_chunks_*` to use
  `resolve_stored_path` for namespaced multi-root paths — is a separate cutover
  task.
- Edge resolution, incremental sync, and tier-2/3 languages are out of scope
  here (Phase 2b/2c).

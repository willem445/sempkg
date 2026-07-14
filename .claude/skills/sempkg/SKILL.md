---
name: sempkg
description: >
  Use sempkg MCP tools to research packages and dependencies with version-accurate
  semantic indexes. Use this skill when asked to look up API symbols, understand
  how a function is used or what it calls, check the downstream impact of changing
  a symbol, read exact source implementations, or answer questions about a specific
  installed library version without risking hallucinated or outdated answers.
---

# sempkg Code Research Skill

This skill gives you access to version-pinned semantic indexes for installed
packages via the sempkg MCP server. The indexes reflect the exact dependency
versions declared in `sempkg.toml` — not generic online documentation that may
describe a different version.

**Never hallucinate API signatures, parameter names, or behaviour.** If a symbol
is not found in the index, say so clearly and suggest a refined search.

---

## Step 1 — Discover available packages

Always start with `list_packages` to see what is indexed. The output shows:
- Package or bundle name and version
- Whether a CodeGraph symbol index is present (`[indexed]`)
- Whether a LanceDB docs index is present (`+lance`)
- Whether a source-code body index is present (`+code`)

---

## Step 2 — Choose the right tool

| Question type | Tool(s) to use |
|---------------|----------------|
| Broad / mixed question (troubleshooting, migration, architecture) | `get_context` then `search_docs` if `+lance` present |
| "What does symbol `X` do / who calls it?" | `search_symbols` → `get_callers` |
| "What does function `X` depend on?" | `get_callees` |
| "What breaks if I change `X`?" | `get_impact` |
| "How do I accomplish [task] in code?" | `get_context` (natural language) |
| "What files are tracked in this package?" | `list_files` |
| "How does [concept] work in this library?" | `search_docs` (requires `+lance`) |
| "Show me the implementation of a symbol I already found" | `read_code(file, line)` or `read_symbol(name)` (requires `+code`) |
| "Find a symbol I don't know the exact name of" | `search_code` (requires `+code`) |

---

## Step 3 — Symbol search tips

- Pass a short keyword to `search_symbols`, not a full sentence.
- Narrow with the `kind` parameter (`function`, `class`, `method`, `variable`,
  `struct`, `enum`, `trait`) when results are too broad.
- Increase `limit` beyond the default 20 if a common name returns too many hits.

---

## Step 4 — Reading exact source code

When `search_symbols`, `get_callers`, `get_callees`, or `get_impact` return a
result that includes a file path and line number, use that information directly
to retrieve the implementation — do **not** do a secondary keyword search:

- **`read_code(package, file, line)`** — preferred. Pass the file path and any
  line number within the symbol (1-based). Returns the tightest enclosing symbol
  body at that location (e.g. the specific method, not the whole class).
  Requires `+code`.
- **`read_symbol(package, symbol)`** — alternative when you have the symbol name
  but no file/line location. Performs an exact name lookup. Requires `+code`.

Use `search_code` only when *discovering* symbols by keyword — not when you
already have a file and line from earlier results.

---

## Step 5 — Call graph exploration

- `get_callers` — all places that call the symbol; useful for understanding
  usage patterns and finding integration points.
- `get_callees` — everything called by the symbol; useful for tracing
  dependencies and side effects.
- `get_impact` — transitive downstream impact; call this before proposing a
  change to understand the blast radius.

---

## Step 6 — Documentation search

- `search_docs` searches the LanceDB full-text index; best for prose
  documentation ("what is the retry policy?", "what are the configuration
  options?").
- Check `docs_metadata` first to confirm the docs index exists and is
  non-empty before calling `search_docs`.

---

## Answer format

1. State the **package name and version** so the user knows the answer is
   version-accurate.
2. Quote exact symbol names, signatures, and file paths from tool output.
3. If a question cannot be answered from the index (symbol not found, docs
   index absent), say so explicitly — do not fill gaps with general knowledge
   that may not match the installed version.
4. When multiple symbols match, list the candidates and ask the user to
   clarify rather than picking one arbitrarily.

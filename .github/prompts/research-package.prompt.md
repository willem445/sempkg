---
name: research-package
description: >
  Research a specific sempkg package: summarise its public API, find relevant
  symbols, and pull documentation for a given topic — all pinned to the exact
  installed version. Use when: onboarding to a new dependency; building a
  feature that uses an external library; verifying an API exists before writing
  code.
---

Research the **`${package}`** package using the sempkg semantic index.

**Topic / task:** ${topic}

Steps:
1. Call `list_packages` to confirm `${package}` is installed and note whether
   it has a docs index (`+lance`).
2. Use `get_context` to retrieve code context relevant to the topic.
3. Use `search_symbols` to find the most relevant symbols for the topic.
   If a specific symbol name is known, also run `get_callers` and `get_callees`
   to show how it fits into the call graph.
4. If the bundle has a `+lance` docs index, run `search_docs` to pull any
   prose documentation related to the topic.
5. Summarise your findings:
   - Package name and version (from `list_packages` output)
   - Key symbols and their signatures
   - How those symbols are used (callers) or what they depend on (callees)
   - Relevant documentation excerpts (if available)
   - Any gaps: symbols not found, docs index absent, etc.

Keep the summary concise. Quote exact names and file paths from the index
rather than paraphrasing — the user needs version-accurate information.

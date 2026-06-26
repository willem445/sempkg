---
name: run-tests
description: >
  Run sempkg Python tests reliably in this repository without re-discovering
  the interpreter, commands, or slow-path caveats each time. Use this skill
  when asked to run unit tests, functional tests, targeted test classes, or
  quick test collection checks.
---

# sempkg Test Runner Skill

Use this skill to run tests in `sempkg` consistently and quickly.

This repo has multiple Python interpreters on some machines. The plain
`python` on PATH may point to Python 3.9 without `pytest`, so always use the
project venv interpreter directly.

---

## Canonical interpreter

From repo root on Windows:

```powershell
.\.venv\Scripts\python.exe
```

Quick sanity check:

```powershell
.\.venv\Scripts\python.exe -c "import pytest; print('pytest ok')"
```

If `.venv` is missing, use `uv` to create/sync it first (project-standard
workflow), then retry.

---

## Fast preflight checks

Run collection before full execution to catch import/syntax errors fast:

```powershell
.\.venv\Scripts\python.exe -m pytest tests/test_mcp_functional.py --collect-only -q
```

Run all tests (typical):

```powershell
.\.venv\Scripts\python.exe -m pytest -q
```

Run a single file:

```powershell
.\.venv\Scripts\python.exe -m pytest tests/test_registry_auth.py -v
```

Run a single class:

```powershell
.\.venv\Scripts\python.exe -m pytest tests/test_mcp_functional.py::TestQueryTool -v --tb=short
```

Run a single test:

```powershell
.\.venv\Scripts\python.exe -m pytest tests/test_mcp_functional.py::TestQueryTool::test_query_scores_above_rrf_floor -v
```

---

## Unit vs functional guidance

- Prefer quick unit-style files first for fast feedback:
  - `tests/test_registry_auth.py`
  - `tests/test_registry_storage.py`
- Run `tests/test_mcp_functional.py` when MCP behavior/output is involved.

`TestQueryTool` in `tests/test_mcp_functional.py` is intentionally expensive
because it boots `sempkg mcp` and executes reranker-backed queries.

---

## Expected slow path

Reranker-backed functional tests can be very slow (tens of minutes) due to:

- Local model load (`qwen3-reranker-0.6b-q8_0.gguf`)
- Two-pass reranking inference across multiple queries

Treat these as integration/perf checks, not quick smoke tests.

---

## Troubleshooting

If you see:

```text
No module named pytest
```

you used the wrong interpreter. Re-run with:

```powershell
.\.venv\Scripts\python.exe -m pytest ...
```

If `uv run pytest ...` fails with trampoline/path errors on Windows, prefer the
direct venv interpreter command above.

If MCP functional tests fail unexpectedly, verify:

- `sempkg` binary is built (`cargo build --release --manifest-path src/sempkg/Cargo.toml`)
- workspace has required bundles/indexes in `sempkg.toml`
- reranker model exists when reranker behavior is under test:

```powershell
Test-Path (Join-Path $HOME ".sempkg\models\qwen3-reranker-0.6b-q8_0.gguf")
```

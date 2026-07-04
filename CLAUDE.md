# sempkg

`sempkg` is a semantic package manager for AI agents. It bundles and serves version-pinned code intelligence (CodeGraph symbol graphs + QMD doc indexes) so agents can query the exact dependency version a project uses.

**Four applications in this repo:**
- `src/sempkg/` — Rust CLI + MCP server (installs bundles, manages workspace state, serves semantic query tools)
- `src/sembundle/` — Rust CLI (packs, signs, validates, and publishes `.sembundle` archives)
- `src/sempkg_registry/` — Python/FastAPI self-hosted registry service
- `src/sempkg_agent/` — Python/LangGraph agent server: a grounded code-intelligence agent over sempkg bundles, exposed via A2A / MCP / REST (own Dockerfile, `docker-compose.yml`, and test suite)

## Common Commands

### Rust
```bash
cargo fmt --all
# sembundle has no optional GPU features, so --all-features is safe there:
cargo clippy --all-targets --all-features --manifest-path src/sembundle/Cargo.toml -- -D warnings
# sempkg's --all-features enables every GPU backend at once (cuda/vulkan/rocm/metal),
# which is mutually incompatible and needs each vendor's SDK — lint the buildable set:
cargo clippy --all-targets --features reranker,embeddings --manifest-path src/sempkg/Cargo.toml -- -D warnings
cargo test
cargo build --release --manifest-path src/sempkg/Cargo.toml
cargo build --release --manifest-path src/sembundle/Cargo.toml
```

### Python
```powershell
uv sync
.\.venv\Scripts\python.exe -m pytest -q
.\.venv\Scripts\python.exe -m pytest tests/test_registry_auth.py -v
.\.venv\Scripts\python.exe -m pytest tests/test_mcp_functional.py -v
```

> `tests/test_mcp_functional.py` boots `sempkg mcp` with a local reranker model — expect minutes-long runs.
> Always use `.venv\Scripts\python.exe` directly; the system `python` may be an incompatible version.

### sempkg_agent (Python)
The agent server has its own package and test suite under `src/sempkg_agent/`.
```bash
uv pip install --system -e "src/sempkg_agent[dev]"
# Fast, fully-offline suite (functional end-to-end tests are opt-in):
python -m pytest src/sempkg_agent/tests -m "not functional"
```

### MCP Server
```bash
sempkg mcp -C .
```

## Code Structure
- `src/sempkg/` — Rust crate: package management + MCP integration
- `src/sembundle/` — Rust crate: bundle creation and distribution
- `src/sempkg_registry/` — Python FastAPI: registry API, auth, storage
- `src/sempkg_agent/` — Python LangGraph agent server (A2A / MCP / REST) with its own `pyproject.toml`, Dockerfile, and `tests/`
- `docs/` — product docs, format specs, architecture notes, roadmaps (planning docs live under `docs/plans/`)
- `tests/` — Python tests (registry auth/storage + MCP functional)
- `scripts/` — repo maintenance scripts

## Development Guidelines

### Rust
- Prefer explicit error handling with `Result` and crate-specific error enums
- Keep modules focused and strongly typed
- Preserve CLI output stability unless changes are explicitly requested
- Run `cargo fmt --all` and `cargo clippy --all-targets -- -D warnings` (per the feature notes in Common Commands above — sempkg uses `--features reranker,embeddings`, sembundle uses `--all-features`) before finalizing
- When validating that code builds, compile with `--release` (e.g. `cargo build --release --manifest-path src/sempkg/Cargo.toml`). Only use a debug build when actively debugging an issue that needs debug symbols/assertions.

### Python
- Target Python 3.11+ patterns; use type hints throughout
- Use `uv` for dependency/environment workflows (`uv sync`, `uv pip`)
- Separate request validation, auth logic, and storage concerns in FastAPI code
- Run `pytest` for modified Python behavior

### General
- Keep changes minimal and scoped to the task
- Update `docs/` when behavior, commands, or architecture meaningfully change
- Document significant architecture decisions in `docs/arch/adr/`
- Avoid breaking changes across the four applications unless explicitly requested

## MCP (sempkg semantic search)
The sempkg MCP server is configured in `.mcp.json`. It exposes version-accurate semantic indexes for installed packages declared in `sempkg.toml`.

Use `/research-package` to look up API symbols, call graphs, or docs pinned to the exact installed versions — not generic online documentation.

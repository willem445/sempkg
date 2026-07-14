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
- See "Build & test policy" below before running any of the above — it governs *who* compiles, *when*, and with what scope.

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

## Build & test policy — read this before you compile anything

Compiling this workspace is expensive: it pulls in Lance, DataFusion, Arrow and
llama-cpp, so a cold build is multi-gigabyte and CPU-saturating. This has
already caused two disk emergencies — the shared cargo target dir reached
**64 GB**, and a single reviewer that built into its own private target dir
cost **8.3 GB** and took the machine to 4 GB free, where cargo starts failing
machine-wide. Treat compilation as a scarce, owned resource. The feature-flag
constraint for `sempkg` (no `--all-features`) is covered above under Common
Commands — this section is about *who* compiles and *when*.

### Reviewers do not compile

If you are reviewing a pull request, you do **not** run `cargo build`,
`cargo test`, `cargo clippy`, or `cargo run`. Your verdict comes from reading
the diff and reading CI:

```bash
gh pr checks <pr>                 # the matrix result
gh run view <run-id> --log-failed # why a job went red
```

CI already runs the full matrix — three platforms, the real feature
combinations, the functional MCP suite. You cannot beat it locally, and
duplicating it buys nothing: a green local run on one machine tells you
strictly less than a green CI run on three.

A red or missing CI run is **the worker's defect to fix, not yours to
reproduce.** Say so in your review and hand it back — that's a finding, not
an errand.

The one narrow exception: testing a hypothesis CI genuinely cannot answer
(e.g. a platform-specific failure you need to bisect). Use
`cargo check -p <crate>` only — never a full build or test run — and state
the reason in the review. If you can't name the reason, you didn't need it.

### Workers validate before handing off

The reviewer is not your test run. Before you open a PR you owe:

- `cargo fmt --all --check`
- `cargo clippy` with the right features (see Common Commands) — the lint job runs `-D warnings`
- the tests covering what you touched
- **red-before-green evidence**: the new test, run against the base branch, failing for the expected reason — command and failure line, in the PR description

### Cheapest sufficient command

| Instead of | Use |
| --- | --- |
| `cargo test --workspace` | `cargo test -p <the crate you touched>` |
| `cargo build` | `cargo check` (when you only need type errors) |
| a full local matrix | let CI do it — that's what it's for |

### One shared target dir, and it is not yours

All agents in a working group **share** one `CARGO_TARGET_DIR`
(`.loomux-target/`, gitignored). This is deliberate: it's what stops N
worktrees from each paying for a cold build.

- **Never set your own `CARGO_TARGET_DIR`.**
- **Never build into a scratchpad**, and never create a private `target-*/`
  directory. That's a from-scratch build of the entire dependency graph —
  the exact mistake that cost 8.3 GB.
- **Never `cargo clean` the shared dir** — you would wipe every other agent's
  cache, including agents mid-build. Cleaning is centralized, done only when
  no agents are live.

Before a big build, check you have room — under ~10 GB free, stop and say so
rather than starting a build that fails at 0 bytes and takes the task board
with it:

```powershell
Get-PSDrive C | Select-Object @{n='FreeGB';e={[math]::Round($_.Free/1GB,1)}}
```

See the `rust-checks` skill for the day-to-day cheat sheet version of this
policy, and the `run-tests` skill for Python test specifics.

## MCP (sempkg semantic search)
The sempkg MCP server is configured in `.mcp.json`. It exposes version-accurate semantic indexes for installed packages declared in `sempkg.toml`.

Use `/research-package` to look up API symbols, call graphs, or docs pinned to the exact installed versions — not generic online documentation.

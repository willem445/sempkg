# Copilot Instructions for sempkg

## Project Background
`sempkg` is a semantic package manager for AI agents. It bundles and serves
version-pinned code intelligence (CodeGraph symbol graphs + QMD-like doc
indexes) so agents can query the exact dependency version a project uses.

The main workflow is:
1. Build or publish `.sembundle` archives.
2. Install bundles into a workspace with `sempkg`.
3. Expose bundle context to coding agents over MCP.

## Applications in This Repository
1. `sempkg` (Rust): CLI + MCP server that installs bundles, manages workspace
state, and serves semantic query tools.
2. `sembundle` (Rust): CLI that packs, signs, validates, verifies, and publishes
`.sembundle` archives.
3. `sempkg-registry` (Python/FastAPI): self-hosted registry service to store and
serve `.sembundle` files.

## Code Structure Overview
- `src/sempkg/`: Rust crate for end-user package management + MCP integration.
- `src/sembundle/`: Rust crate for creating and distributing semantic bundles.
- `src/sempkg_registry/`: Python service for registry API, auth, and storage.
- `docs/`: product docs, format specs, architecture notes, and roadmaps.
- `tests/`: Python tests for registry auth/storage behavior.
- `scripts/`: repo maintenance scripts (for example, version bumping).

## Development Guidance

### Rust best practices
- Keep modules focused and strongly typed; prefer explicit error handling with
`Result` and crate-specific error enums.
- Preserve CLI behavior and output stability unless changes are requested.
- Add/update tests for changed behavior where practical.
- Run formatting/lints/tests before finalizing changes:
	- `cargo fmt`
	- `cargo clippy --all-targets --all-features -- -D warnings`
	- `cargo test`

### Python best practices
- Target Python 3.11+ patterns already used by this repo.
- Use type hints and clear function boundaries in FastAPI app/auth/storage code.
- Prefer `uv` for dependency/environment workflows (`uv sync`, `uv pip ...`).
- Keep request validation, auth logic, and storage concerns separated.
- Run tests for modified Python behavior (`pytest`).

### Cross-cutting guidance
- Keep changes minimal and scoped to the task.
- Update docs when behavior, commands, or architecture meaningfully change.
- Avoid introducing breaking changes across the three applications unless explicitly requested.
- Important architecture decision or changes should be documented in `docs/arch/adr/` or relevant design docs.

# Copilot Instructions for sempkg

## Project context
- This repository contains the `sempkg` Rust CLI/MCP server and the `sempkg-registry` Python server.
- `sempkg` is a semantic package manager: it installs and queries `.sembundle` semantic index archives and exposes them via MCP tools to GitHub Copilot.
- `sembundle` is the Rust CLI for packing, signing, and publishing `.sembundle` archives.
- `sempkg-registry` is a self-hosted FastAPI server for storing and serving `.sembundle` files.

## Development preferences
- Prefer `uv` for Python package management and environment tasks.
- Use `uv pip` instead of `pip` when possible (install, editable install, add dependencies).
- Keep commands and examples `uv`-first unless compatibility requires another tool.

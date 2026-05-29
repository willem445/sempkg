# Copilot Instructions for codegraph-hub

## Project context
- This package is `codegraph-hub`, a multi-repo MCP server for GitHub Copilot.
- It registers locally cloned internal Python packages and indexes them with `codegraph`.
- It exposes one MCP endpoint for querying symbols, call graphs, and source files across registered packages.

## Development preferences
- Prefer `uv` for Python package management and environment tasks.
- Use `uv pip` instead of `pip` when possible (install, editable install, add dependencies).
- Keep commands and examples `uv`-first unless compatibility requires another tool.

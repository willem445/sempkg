"""MCP server exposing multi-repo codegraph tools to GitHub Copilot."""

import textwrap
from pathlib import Path

from fastmcp import FastMCP

from .bundle_store import get_global_store, get_workspace_store
from .codegraph import (
    callers,
    callees,
    context,
    files,
    find_symbols,
    impact,
    init_and_index,
    query,
    read_symbol_source,
    run_across_packages,
    status,
    sync,
)
from .registry import Registry

mcp = FastMCP(
    "codegraph-hub",
    instructions=textwrap.dedent("""\
        codegraph-hub indexes your internal Python packages and makes their
        symbols, call graphs, and source code available without file-reading.

        Workflow:
        1. Call list_packages to see what internal packages are available.
        2. Call search_package to find a specific class, function, or module.
        3. Call get_context with a task description to get AI-optimized context
           about how to use the package for a specific purpose.
        4. Call get_callers / get_callees to understand call relationships.
        5. Call read_file for the full source of a specific file when needed.

        Always start with list_packages if you are unsure which package contains
        the symbol you are looking for.
    """),
)

registry = Registry()


def _require(package_name: str):
    """Return a Package or a user-facing error string."""
    pkg = registry.get(package_name)
    if pkg is None:
        pkgs = [p.name for p in registry.list_all()]
        hint = f" Available packages: {', '.join(pkgs)}" if pkgs else " No packages registered yet."
        return None, f"Package '{package_name}' not found.{hint}"
    if not pkg.is_indexed:
        return None, (
            f"Package '{package_name}' is registered but not yet indexed. "
            f"Use reindex_package('{package_name}') to index it."
        )
    return pkg, None


# ---------------------------------------------------------------------------
# Discovery / management tools
# ---------------------------------------------------------------------------


@mcp.tool()
def list_packages() -> str:
    """List all registered internal packages and their index status."""
    packages = registry.list_all()
    if not packages:
        return (
            "No internal packages registered.\n"
            "Use add_package(name, path) to register a locally cloned internal repo."
        )
    lines = ["Registered internal packages:\n"]
    for pkg in packages:
        status_icon = "✓ indexed" if pkg.is_indexed else "✗ not indexed (run reindex_package)"
        desc = f" — {pkg.description}" if pkg.description else ""
        lines.append(f"  • **{pkg.name}** [{status_icon}]{desc}")
        lines.append(f"    path: {pkg.path}")
    return "\n".join(lines)


@mcp.tool()
def add_package(name: str, path: str, description: str = "") -> str:
    """Register a locally cloned internal repo and index it with codegraph.

    Args:
        name: Short identifier (e.g. "pandas", "mylib").
        path: Absolute or ~ path to the cloned repository root.
        description: One-line summary of what the package provides.
    """
    try:
        pkg = registry.add(name, path, description)
    except ValueError as exc:
        return str(exc)

    if pkg.is_indexed:
        return (
            f"Package '{name}' registered. Index already exists at {pkg.path}.\n"
            f"Use reindex_package('{name}') to refresh it."
        )

    try:
        out = init_and_index(pkg.abs_path)
        return f"Package '{name}' registered and indexed successfully.\n{out}"
    except RuntimeError as exc:
        return (
            f"Package '{name}' registered at {pkg.path}, but indexing failed:\n{exc}\n"
            f"Fix the error above then call reindex_package('{name}')."
        )


@mcp.tool()
def remove_package(package_name: str) -> str:
    """Remove a package from the registry (does not delete the repo or its index).

    Args:
        package_name: Name of the package to remove.
    """
    if registry.remove(package_name):
        return f"Package '{package_name}' removed from registry."
    return f"Package '{package_name}' not found."


@mcp.tool()
def reindex_package(package_name: str) -> str:
    """Re-index a registered package to pick up new commits or file changes.

    Args:
        package_name: Name of the registered package to re-index.
    """
    pkg = registry.get(package_name)
    if pkg is None:
        return f"Package '{package_name}' not found."
    try:
        if pkg.is_indexed:
            out = sync(pkg.abs_path)
        else:
            out = init_and_index(pkg.abs_path)
        return f"Package '{package_name}' re-indexed.\n{out}"
    except RuntimeError as exc:
        return f"Re-indexing failed: {exc}"


@mcp.tool()
def package_status(package_name: str) -> str:
    """Show codegraph index statistics for a registered package.

    Args:
        package_name: Name of the registered package.
    """
    pkg = registry.get(package_name)
    if pkg is None:
        return f"Package '{package_name}' not found."
    return status(pkg.abs_path)


# ---------------------------------------------------------------------------
# Code intelligence tools
# ---------------------------------------------------------------------------


@mcp.tool()
def search_package(package_name: str, query_str: str, kind: str = "") -> str:
    """Search for symbols (functions, classes, methods, modules) in an internal package.

    Args:
        package_name: Name of the registered internal package, or "" to search all packages.
        query_str: Symbol name or keyword to search for.
        kind: Optional type filter — one of: function, class, method, module, variable.
    """
    if not package_name:
        pkgs = registry.list_all()
        return run_across_packages(pkgs, lambda p: query(p.abs_path, query_str, kind or None))
    pkg, err = _require(package_name)
    if err:
        return err
    return query(pkg.abs_path, query_str, kind or None)


@mcp.tool()
def get_context(package_name: str, task: str) -> str:
    """Get AI-optimized context for a task or question about an internal package.

    Returns entry points, related symbols, and code snippets relevant to the task.

    Args:
        package_name: Name of the registered internal package, or "" to query all packages.
        task: What you're trying to understand or do, e.g.
              "how to write a test case", "TestSuite base class and fixtures".
    """
    if not package_name:
        pkgs = registry.list_all()
        return run_across_packages(pkgs, lambda p: context(p.abs_path, task))
    pkg, err = _require(package_name)
    if err:
        return err
    return context(pkg.abs_path, task)


@mcp.tool()
def get_callers(package_name: str, symbol: str, limit: int = 20) -> str:
    """Find all callers of a function or method within an internal package.

    Args:
        package_name: Name of the registered internal package, or "" to search all.
        symbol: Function or method name.
        limit: Maximum number of results (default 20).
    """
    if not package_name:
        return run_across_packages(registry.list_all(), lambda p: callers(p.abs_path, symbol, limit))
    pkg, err = _require(package_name)
    if err:
        return err
    return callers(pkg.abs_path, symbol, limit)


@mcp.tool()
def get_callees(package_name: str, symbol: str, limit: int = 20) -> str:
    """Find everything a specific function or method calls inside an internal package.

    Args:
        package_name: Name of the registered internal package, or "" to search all.
        symbol: Function or method name to inspect.
        limit: Maximum number of results (default 20).
    """
    if not package_name:
        return run_across_packages(registry.list_all(), lambda p: callees(p.abs_path, symbol, limit))
    pkg, err = _require(package_name)
    if err:
        return err
    return callees(pkg.abs_path, symbol, limit)


@mcp.tool()
def get_impact(package_name: str, symbol: str, depth: int = 3) -> str:
    """Analyze what code would be affected by changing a symbol in an internal package.

    Args:
        package_name: Name of the registered internal package, or "" to search all.
        symbol: Symbol (function, class, method) to analyze.
        depth: How many levels deep to trace impact (default 3).
    """
    if not package_name:
        return run_across_packages(registry.list_all(), lambda p: impact(p.abs_path, symbol, depth))
    pkg, err = _require(package_name)
    if err:
        return err
    return impact(pkg.abs_path, symbol, depth)


@mcp.tool()
def list_package_files(package_name: str, filter_pattern: str = "") -> str:
    """List the file structure of an internal package.

    Args:
        package_name: Name of the registered internal package, or "" to list all.
        filter_pattern: Optional glob pattern to filter files (e.g. "*.py", "tests/").
    """
    if not package_name:
        return run_across_packages(registry.list_all(), lambda p: files(p.abs_path, filter_pattern or None))
    pkg, err = _require(package_name)
    if err:
        return err
    return files(pkg.abs_path, filter_pattern or None)


@mcp.tool()
def read_file(package_name: str, file_path: str) -> str:
    """Read the full source of a specific file from an internal package.

    Use this after search_package or get_context to get a complete function or
    class implementation.

    Args:
        package_name: Name of the registered internal package.
        file_path: Relative path within the package root
               (e.g. "pandas/runner.py", "tests/base.py").
    """
    pkg = registry.get(package_name)
    if pkg is None:
        return f"Package '{package_name}' not found."

    full_path = pkg.abs_path / file_path

    # Guard against path traversal
    try:
        full_path.resolve().relative_to(pkg.abs_path.resolve())
    except ValueError:
        return "Error: file path escapes the package directory."

    if not full_path.exists():
        return f"File not found: {file_path}"
    if not full_path.is_file():
        return f"Not a file: {file_path}"

    try:
        content = full_path.read_text(encoding="utf-8")
        suffix = full_path.suffix.lstrip(".")
        lang = suffix if suffix else "text"
        return f"# {package_name}/{file_path}\n\n```{lang}\n{content}\n```"
    except Exception as exc:
        return f"Error reading file: {exc}"


@mcp.tool()
def read_symbol(package_name: str, symbol_name: str, kind: str = "", context_lines: int = 0) -> str:
    """Read the full source of a specific symbol (function, class, method) from an internal package.

    Queries the codegraph index to find the exact file and line range — no need to
    know the file path in advance. Use this after search_package returns a symbol name.

    Args:
        package_name: Name of the registered internal package, or "" to search all packages
                      (stops at the first package that contains the symbol).
        symbol_name: Exact name of the symbol (function, class, method, etc.).
        kind: Optional — narrow to a specific kind: function, class, method, module, variable.
        context_lines: Extra lines of surrounding context to include above/below (default 0).
    """
    def _fetch(pkg):
        matches = find_symbols(pkg.abs_path, symbol_name, kind or None)
        if not matches:
            return ""
        return "\n\n---\n\n".join(read_symbol_source(pkg.abs_path, m, context_lines) for m in matches)

    if not package_name:
        return run_across_packages(registry.list_all(), _fetch, stop_on_first=True)

    pkg = registry.get(package_name)
    if pkg is None:
        return f"Package '{package_name}' not found."
    if not pkg.is_indexed:
        return f"Package '{package_name}' is not indexed."
    result = _fetch(pkg)
    if not result:
        return (
            f"Symbol '{symbol_name}' not found in '{package_name}'.\n"
            f"Try search_package('{package_name}', '{symbol_name}') to find similar names."
        )
    return result


@mcp.tool()
def read_file_range(package_name: str, file_path: str, start_line: int, end_line: int) -> str:
    """Read a specific line range from a file in an internal package.

    Useful when you know the exact location from a search result and want to
    pull a tight slice without fetching the whole file.

    Args:
        package_name: Name of the registered internal package.
        file_path: Relative path within the package (e.g. "pandas/runner.py").
        start_line: First line to return (1-indexed, inclusive).
        end_line: Last line to return (1-indexed, inclusive).
    """
    pkg = registry.get(package_name)
    if pkg is None:
        return f"Package '{package_name}' not found."

    full_path = pkg.abs_path / file_path
    try:
        full_path.resolve().relative_to(pkg.abs_path.resolve())
    except ValueError:
        return "Error: file path escapes the package directory."

    if not full_path.exists():
        return f"File not found: {file_path}"

    lines = full_path.read_text(encoding="utf-8", errors="replace").splitlines()
    total = len(lines)
    s = max(0, start_line - 1)
    e = min(total, end_line)
    snippet = "\n".join(lines[s:e])
    lang = full_path.suffix.lstrip(".") or "text"
    return f"# {package_name}/{file_path}  (lines {start_line}–{end_line} of {total})\n\n```{lang}\n{snippet}\n```"


# ---------------------------------------------------------------------------
# Bundle tools (pre-built .cgbundle indexes)
# ---------------------------------------------------------------------------

def _resolve_bundle(name: str, version: str, workspace_dir: str) -> Path | None:
    """Try workspace store first, then global. Return extracted bundle dir or None."""
    ws_store = get_workspace_store(Path(workspace_dir) if workspace_dir else None)
    bundle_dir = ws_store.resolve(name, version)
    if bundle_dir is not None:
        return bundle_dir
    return get_global_store().resolve(name, version)


@mcp.tool()
def list_bundle_packages(workspace_dir: str = "") -> str:
    """List all installed .cgbundle packages available in the workspace and globally.

    Workspace bundles are scoped to the current project (.codegraph_hub/ in workspace_dir).
    Global bundles (~/.codegraph_hub/bundles/) are available across all workspaces.
    """
    try:
        ws_store = get_workspace_store(Path(workspace_dir) if workspace_dir else None)
        ws_bundles = ws_store.list_installed()
        global_bundles = get_global_store().list_installed()
    except Exception as exc:
        return f"Error listing bundles: {exc}"

    if not ws_bundles and not global_bundles:
        return (
            "No installed bundles found.\n"
            "Use 'cgbundle-hub bundle install <name>@<version>' to install a bundle."
        )

    lines: list[str] = []
    if ws_bundles:
        lines.append("Workspace bundles:\n")
        for b in ws_bundles:
            lines.append(f"  • **{b.name}** @ {b.version}  [workspace]")
            lines.append(f"    path: {b.store_dir}")
    if global_bundles:
        lines.append("\nGlobal bundles:\n")
        for b in global_bundles:
            lines.append(f"  • **{b.name}** @ {b.version}  [global]")
            lines.append(f"    path: {b.store_dir}")
    return "\n".join(lines)


@mcp.tool()
def get_bundle_info(name: str, version: str, workspace_dir: str = "") -> str:
    """Get detailed information about an installed bundle including its manifest.

    Checks workspace bundles first, then global bundles.
    """
    try:
        ws_store = get_workspace_store(Path(workspace_dir) if workspace_dir else None)
        found: object = None
        scope = ""
        for b in ws_store.list_installed():
            if b.name == name and b.version == version:
                found = b
                scope = "workspace"
                break
        if found is None:
            for b in get_global_store().list_installed():
                if b.name == name and b.version == version:
                    found = b
                    scope = "global"
                    break
    except Exception as exc:
        return f"Error looking up bundle: {exc}"

    if found is None:
        return f"Bundle '{name}@{version}' not found. Use list_bundle_packages to see available bundles."

    m = found.manifest
    lines = [
        f"**{found.name}** @ {found.version}  [{scope}]",
        f"  path:              {found.store_dir}",
        f"  source_repo:       {m.get('source_repo', 'n/a')}",
        f"  commit_hash:       {m.get('commit_hash', 'n/a')}",
        f"  created_at:        {m.get('created_at', 'n/a')}",
        f"  codegraph_version: {m.get('codegraph_version', 'n/a')}",
        f"  extensions:        {', '.join(m.get('extensions', [])) or 'none'}",
    ]
    return "\n".join(lines)


@mcp.tool()
def search_bundle_symbol(
    bundle_name: str, bundle_version: str, query_str: str, workspace_dir: str = ""
) -> str:
    """Search for a symbol in an installed bundle's CodeGraph index.

    Bundles contain pre-built CodeGraph indexes. Workspace bundles are only
    accessible when querying from within that workspace. Global bundles are
    accessible from any workspace.

    Use list_bundle_packages to see available bundles.
    """
    bundle_dir = _resolve_bundle(bundle_name, bundle_version, workspace_dir)
    if bundle_dir is None:
        return (
            f"Bundle '{bundle_name}@{bundle_version}' not found. "
            "Use list_bundle_packages to see available bundles."
        )
    try:
        return query(bundle_dir, query_str, kind=None, limit=10)
    except Exception as exc:
        return f"Error querying bundle: {exc}"


@mcp.tool()
def list_bundle_callers(
    bundle_name: str, bundle_version: str, symbol: str, workspace_dir: str = ""
) -> str:
    """List callers of a symbol in an installed bundle's CodeGraph index."""
    bundle_dir = _resolve_bundle(bundle_name, bundle_version, workspace_dir)
    if bundle_dir is None:
        return (
            f"Bundle '{bundle_name}@{bundle_version}' not found. "
            "Use list_bundle_packages to see available bundles."
        )
    try:
        return callers(bundle_dir, symbol)
    except Exception as exc:
        return f"Error querying bundle: {exc}"


@mcp.tool()
def list_bundle_callees(
    bundle_name: str, bundle_version: str, symbol: str, workspace_dir: str = ""
) -> str:
    """List callees (functions called by) a symbol in an installed bundle's CodeGraph index."""
    bundle_dir = _resolve_bundle(bundle_name, bundle_version, workspace_dir)
    if bundle_dir is None:
        return (
            f"Bundle '{bundle_name}@{bundle_version}' not found. "
            "Use list_bundle_packages to see available bundles."
        )
    try:
        return callees(bundle_dir, symbol)
    except Exception as exc:
        return f"Error querying bundle: {exc}"


def main() -> None:
    mcp.run()

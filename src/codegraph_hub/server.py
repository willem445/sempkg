"""MCP server exposing multi-repo codegraph tools to GitHub Copilot."""

import textwrap
from pathlib import Path

from fastmcp import FastMCP

from .bundle_store import get_all_bundle_packages, get_global_store, get_workspace_store
from . import lance as lance_mod
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
        codegraph-hub exposes CodeGraph symbol/call-graph intelligence and LanceDB
        documentation search for internally registered packages and installed
        .cgbundle archives.

        Workflow:
        1. Call list_packages to see registered packages AND installed bundles.
        2. Call search_package to find a class, function, or module by name.
        3. Call get_context with a task description for AI-optimized code context.
        4. Call get_callers / get_callees to understand call relationships.
        5. Call search_bundle_docs to search bundled documentation (LanceDB BM25).
        6. Call read_file for the full source of a specific file (packages only).

        Always start with list_packages when unsure which package contains a symbol.
        Use search_bundle_docs when looking for documentation, guides, or prose.
    """),
)

registry = Registry()


def _require(package_name: str):
    """Return a Package/BundlePackage or a user-facing error string.

    Checks the Registry first, then workspace bundle store, then global bundle
    store. For bundles, the first (workspace-preferenced) match by name is used.
    """
    pkg = registry.get(package_name)
    if pkg is not None:
        if not pkg.is_indexed:
            return None, (
                f"Package '{package_name}' is registered but not yet indexed. "
                f"Use reindex_package('{package_name}') to index it."
            )
        return pkg, None

    # Check bundle stores (workspace first)
    for bp in get_all_bundle_packages():
        if bp.name == package_name:
            if not bp.is_indexed:
                return None, (
                    f"Bundle '{package_name}@{bp.version}' is installed but not queryable "
                    "(missing graph/ directory)."
                )
            return bp, None

    # Not found anywhere
    reg_names = [p.name for p in registry.list_all()]
    bundle_names = [f"{b.name}@{b.version}" for b in get_all_bundle_packages()]
    all_names = reg_names + bundle_names
    hint = f" Available: {', '.join(all_names)}" if all_names else " No packages or bundles registered yet."
    return None, f"Package '{package_name}' not found.{hint}"


# ---------------------------------------------------------------------------
# Discovery / management tools
# ---------------------------------------------------------------------------


@mcp.tool()
def list_packages() -> str:
    """List all registered internal packages and installed bundles with their index status."""
    packages = registry.list_all()
    bundle_pkgs = get_all_bundle_packages()

    if not packages and not bundle_pkgs:
        return (
            "No internal packages or bundles registered.\n"
            "  Packages: use add_package(name, path) to register a locally cloned repo.\n"
            "  Bundles:  run 'codegraph-hub bundle sync' or 'codegraph-hub bundle add <pkg>@<ver>'."
        )

    lines: list[str] = []

    if packages:
        lines.append("**Registered packages:**\n")
        for pkg in packages:
            status_icon = "✓ indexed" if pkg.is_indexed else "✗ not indexed (run reindex_package)"
            desc = f" — {pkg.description}" if pkg.description else ""
            lines.append(f"  • **{pkg.name}** [{status_icon}]{desc}")
            lines.append(f"    path: {pkg.path}")

    if bundle_pkgs:
        lines.append("\n**Installed bundles:**\n")
        for bp in bundle_pkgs:
            status_icon = "✓ indexed" if bp.is_indexed else "✗ not queryable"
            lance_flag = "  *(+Lance docs)*" if bp.has_lance() else ""
            scope = "workspace" if ".codegraph_hub" in str(bp.abs_path) else "global"
            lines.append(f"  • **{bp.name}** @ {bp.version}  [{status_icon}]  [{scope}]{lance_flag}")
            lines.append(f"    path: {bp.path}")

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
    """Search for symbols (functions, classes, methods, modules) in a package or bundle.

    Args:
        package_name: Registered package name or installed bundle name, or "" to search all.
        query_str: Symbol name or keyword to search for.
        kind: Optional type filter — one of: function, class, method, module, variable.
    """
    if not package_name:
        all_sources = list(registry.list_all()) + get_all_bundle_packages()
        return run_across_packages(all_sources, lambda p: query(p.abs_path, query_str, kind or None))
    pkg, err = _require(package_name)
    if err:
        return err
    return query(pkg.abs_path, query_str, kind or None)


@mcp.tool()
def get_context(package_name: str, task: str) -> str:
    """Get AI-optimized context for a task or question about a package or bundle.

    Returns entry points, related symbols, and code snippets relevant to the task.

    Args:
        package_name: Registered package or bundle name, or "" to query all.
        task: What you're trying to understand or do, e.g.
              "how to write a test case", "TestSuite base class and fixtures".
    """
    if not package_name:
        all_sources = list(registry.list_all()) + get_all_bundle_packages()
        return run_across_packages(all_sources, lambda p: context(p.abs_path, task))
    pkg, err = _require(package_name)
    if err:
        return err
    return context(pkg.abs_path, task)


@mcp.tool()
def get_callers(package_name: str, symbol: str, limit: int = 20) -> str:
    """Find all callers of a function or method within a package or bundle.

    Args:
        package_name: Registered package or bundle name, or "" to search all.
        symbol: Function or method name.
        limit: Maximum number of results (default 20).
    """
    if not package_name:
        return run_across_packages(
            list(registry.list_all()) + get_all_bundle_packages(),
            lambda p: callers(p.abs_path, symbol, limit),
        )
    pkg, err = _require(package_name)
    if err:
        return err
    return callers(pkg.abs_path, symbol, limit)


@mcp.tool()
def get_callees(package_name: str, symbol: str, limit: int = 20) -> str:
    """Find everything a specific function or method calls inside a package or bundle.

    Args:
        package_name: Registered package or bundle name, or "" to search all.
        symbol: Function or method name to inspect.
        limit: Maximum number of results (default 20).
    """
    if not package_name:
        return run_across_packages(
            list(registry.list_all()) + get_all_bundle_packages(),
            lambda p: callees(p.abs_path, symbol, limit),
        )
    pkg, err = _require(package_name)
    if err:
        return err
    return callees(pkg.abs_path, symbol, limit)


@mcp.tool()
def get_impact(package_name: str, symbol: str, depth: int = 3) -> str:
    """Analyze what code would be affected by changing a symbol in a package or bundle.

    Args:
        package_name: Registered package or bundle name, or "" to search all.
        symbol: Symbol (function, class, method) to analyze.
        depth: How many levels deep to trace impact (default 3).
    """
    if not package_name:
        return run_across_packages(
            list(registry.list_all()) + get_all_bundle_packages(),
            lambda p: impact(p.abs_path, symbol, depth),
        )
    pkg, err = _require(package_name)
    if err:
        return err
    return impact(pkg.abs_path, symbol, depth)


@mcp.tool()
def list_package_files(package_name: str, filter_pattern: str = "") -> str:
    """List the file structure of a package or bundle.

    Args:
        package_name: Registered package or bundle name, or "" to list all.
        filter_pattern: Optional glob pattern to filter files (e.g. "*.py", "tests/").
    """
    if not package_name:
        return run_across_packages(
            list(registry.list_all()) + get_all_bundle_packages(),
            lambda p: files(p.abs_path, filter_pattern or None),
        )
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
    """Read the full source of a specific symbol from a registered package.

    Queries the codegraph index to find the exact file and line range — no need to
    know the file path in advance. Use this after search_package returns a symbol name.

    Note: symbol source reading requires access to the original source files and is
    only available for registered packages, not for installed bundles. Use
    search_package for bundles.

    Args:
        package_name: Name of the registered package, or "" to search all packages
                      (stops at the first package that contains the symbol).
        symbol_name: Exact name of the symbol (function, class, method, etc.).
        kind: Optional — narrow to a specific kind: function, class, method, module, variable.
        context_lines: Extra lines of surrounding context to include above/below (default 0).
    """
    from .bundle_store import BundlePackage as _BundlePackage

    def _fetch(pkg):
        if isinstance(pkg, _BundlePackage):
            # No source files in bundles — fall back to CLI symbol search
            matches = find_symbols(pkg.abs_path, symbol_name, kind or None)
            if matches:
                return "\n\n---\n\n".join(
                    read_symbol_source(pkg.abs_path, m, context_lines) for m in matches
                )
            return ""
        matches = find_symbols(pkg.abs_path, symbol_name, kind or None)
        if not matches:
            return ""
        return "\n\n---\n\n".join(read_symbol_source(pkg.abs_path, m, context_lines) for m in matches)

    if not package_name:
        all_sources = list(registry.list_all()) + get_all_bundle_packages()
        return run_across_packages(all_sources, _fetch, stop_on_first=True)

    pkg_or_err = registry.get(package_name)
    if pkg_or_err is None:
        # Check bundles
        for bp in get_all_bundle_packages():
            if bp.name == package_name:
                if not bp.is_indexed:
                    return f"Bundle '{package_name}' is not queryable."
                result = _fetch(bp)
                if not result:
                    return (
                        f"Symbol '{symbol_name}' not found in bundle '{package_name}'.\n"
                        f"Try search_package('{package_name}', '{symbol_name}') to find similar names."
                    )
                return result
        return f"Package '{package_name}' not found."
    pkg = pkg_or_err
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



@mcp.tool()
def search_bundle_docs(
    bundle_name: str,
    query_str: str,
    bundle_version: str = "",
    workspace_dir: str = "",
    limit: int = 10,
) -> str:
    """Search the LanceDB documentation index of an installed bundle.

    Performs BM25 full-text search over documentation files (markdown, rst, txt)
    that were indexed into the bundle's LanceDB table during packing.

    Workspace bundles are checked first, then global bundles.

    Args:
        bundle_name:     Bundle name to search, or "" to search all bundles with Lance.
        query_str:       Documentation search query (keywords or natural language).
        bundle_version:  Specific version to target (optional; uses first match if omitted).
        workspace_dir:   Workspace root for scoped bundle resolution (default: cwd).
        limit:           Maximum number of results (default 10).
    """
    ws_dir = Path(workspace_dir) if workspace_dir else None
    bundle_pkgs = get_all_bundle_packages(ws_dir)

    if not bundle_name:
        lance_bundles = [bp for bp in bundle_pkgs if bp.has_lance()]
        if not lance_bundles:
            return (
                "No installed bundles have a LanceDB documentation index.\n"
                "Bundles must be packed with the --lance-dir option to include a Lance index."
            )
        sections: list[str] = []
        for bp in lance_bundles:
            result = lance_mod.lance_search(bp.abs_path, query_str, limit)
            if result and not result.startswith("(no results"):
                sections.append(f"### {bp.name} @ {bp.version}\n\n{result}")
        if not sections:
            return f"No LanceDB documentation results for '{query_str}' across installed bundles."
        return "\n\n---\n\n".join(sections)

    # Find a specific bundle by name (and optionally version)
    target = None
    for bp in bundle_pkgs:
        if bp.name == bundle_name and (not bundle_version or bp.version == bundle_version):
            target = bp
            break

    if target is None:
        spec = f"{bundle_name}@{bundle_version}" if bundle_version else bundle_name
        return (
            f"Bundle '{spec}' not found. "
            "Use list_bundle_packages to see available bundles."
        )
    if not target.has_lance():
        return (
            f"Bundle '{target.name}@{target.version}' does not have a LanceDB documentation index.\n"
            "Bundles must be packed with the --lance-dir option to include a Lance index."
        )
    return lance_mod.lance_search(target.abs_path, query_str, limit)


def main() -> None:
    mcp.run()

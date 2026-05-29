"""Command-line interface for managing the codegraph-hub package registry."""

import argparse
import sys

from . import codegraph as cg
from .registry import Registry


# ---------------------------------------------------------------------------
# Package management commands
# ---------------------------------------------------------------------------

def cmd_list(registry: Registry, args: argparse.Namespace) -> int:
    packages = registry.list_all()
    if not packages:
        print("No packages registered. Use 'codegraph-hub add <name> <path>' to add one.")
        return 0
    for pkg in packages:
        status = "indexed" if pkg.is_indexed else "NOT indexed"
        desc = f"  # {pkg.description}" if pkg.description else ""
        print(f"  {pkg.name:<20} [{status}]  {pkg.path}{desc}")
    return 0


def cmd_add(registry: Registry, args: argparse.Namespace) -> int:
    try:
        pkg = registry.add(args.name, args.path, args.description or "")
    except ValueError as exc:
        print(f"Error: {exc}", file=sys.stderr)
        return 1

    if pkg.is_indexed:
        print(f"Registered '{args.name}' (already indexed at {pkg.path}).")
        print(f"Run 'codegraph-hub reindex {args.name}' to refresh.")
        return 0

    print(f"Registered '{args.name}'. Indexing {pkg.path} ...")
    try:
        out = cg.init_and_index(pkg.abs_path)
        print(out)
        print(f"Done. '{args.name}' is ready.")
        return 0
    except RuntimeError as exc:
        print(f"Indexing failed: {exc}", file=sys.stderr)
        print(f"Fix the error above then run 'codegraph-hub reindex {args.name}'.", file=sys.stderr)
        return 1


def cmd_remove(registry: Registry, args: argparse.Namespace) -> int:
    if registry.remove(args.name):
        print(f"Removed '{args.name}' from registry (repo and index files untouched).")
        return 0
    print(f"Package '{args.name}' not found.", file=sys.stderr)
    return 1


def cmd_reindex(registry: Registry, args: argparse.Namespace) -> int:
    pkg = registry.get(args.name)
    if pkg is None:
        print(f"Package '{args.name}' not found.", file=sys.stderr)
        return 1
    print(f"Re-indexing '{args.name}' at {pkg.path} ...")
    try:
        out = cg.sync(pkg.abs_path) if pkg.is_indexed else cg.init_and_index(pkg.abs_path)
        print(out)
        print("Done.")
        return 0
    except RuntimeError as exc:
        print(f"Failed: {exc}", file=sys.stderr)
        return 1


def cmd_status(registry: Registry, args: argparse.Namespace) -> int:
    pkg = registry.get(args.name)
    if pkg is None:
        print(f"Package '{args.name}' not found.", file=sys.stderr)
        return 1
    print(cg.status(pkg.abs_path))
    return 0


# ---------------------------------------------------------------------------
# Query commands
# ---------------------------------------------------------------------------

def _require_indexed(registry: Registry, name: str):
    """Return (pkg, error_code). Prints error and returns (None, 1) on failure."""
    pkg = registry.get(name)
    if pkg is None:
        print(f"Package '{name}' not found. Use 'codegraph-hub list' to see registered packages.", file=sys.stderr)
        return None, 1
    if not pkg.is_indexed:
        print(f"Package '{name}' is not indexed. Run 'codegraph-hub reindex {name}' first.", file=sys.stderr)
        return None, 1
    return pkg, 0


def _all_indexed(registry: Registry) -> list:
    pkgs = [p for p in registry.list_all() if p.is_indexed]
    if not pkgs:
        print("No indexed packages. Use 'codegraph-hub add <name> <path>' to register one.", file=sys.stderr)
    return pkgs


def cmd_search(registry: Registry, args: argparse.Namespace) -> int:
    kind = getattr(args, "kind", None) or None
    if not args.package:
        pkgs = _all_indexed(registry)
        if not pkgs:
            return 1
        print(cg.run_across_packages(pkgs, lambda p: cg.query(p.abs_path, args.query, kind, args.limit)))
        return 0
    pkg, rc = _require_indexed(registry, args.package)
    if rc:
        return rc
    print(cg.query(pkg.abs_path, args.query, kind, args.limit))
    return 0


def cmd_context(registry: Registry, args: argparse.Namespace) -> int:
    if not args.package:
        pkgs = _all_indexed(registry)
        if not pkgs:
            return 1
        print(cg.run_across_packages(pkgs, lambda p: cg.context(p.abs_path, args.task)))
        return 0
    pkg, rc = _require_indexed(registry, args.package)
    if rc:
        return rc
    print(cg.context(pkg.abs_path, args.task))
    return 0


def cmd_symbol(registry: Registry, args: argparse.Namespace) -> int:
    kind = getattr(args, "kind", None) or None
    context_lines = getattr(args, "context", 0) or 0

    if not args.package:
        pkgs = _all_indexed(registry)
        if not pkgs:
            return 1
        # stop_on_first=True: return the first package that has the symbol
        result = cg.run_across_packages(
            pkgs,
            lambda p: _symbol_source(p, args.symbol, kind, context_lines),
            stop_on_first=True,
        )
        print(result)
        return 0 if result != "(no results in any package)" else 1

    pkg, rc = _require_indexed(registry, args.package)
    if rc:
        return rc
    out = _symbol_source(pkg, args.symbol, kind, context_lines)
    if not out:
        print(f"Symbol '{args.symbol}' not found in '{args.package}'.", file=sys.stderr)
        print(f"Try: codegraph-hub search {args.package} {args.symbol}", file=sys.stderr)
        return 1
    print(out)
    return 0


def _symbol_source(pkg, symbol: str, kind, context_lines: int) -> str:
    matches = cg.find_symbols(pkg.abs_path, symbol, kind)
    if not matches:
        return ""
    return "\n\n---\n\n".join(cg.read_symbol_source(pkg.abs_path, m, context_lines) for m in matches)


def cmd_callers(registry: Registry, args: argparse.Namespace) -> int:
    if not args.package:
        pkgs = _all_indexed(registry)
        if not pkgs:
            return 1
        print(cg.run_across_packages(pkgs, lambda p: cg.callers(p.abs_path, args.symbol, args.limit)))
        return 0
    pkg, rc = _require_indexed(registry, args.package)
    if rc:
        return rc
    print(cg.callers(pkg.abs_path, args.symbol, args.limit))
    return 0


def cmd_callees(registry: Registry, args: argparse.Namespace) -> int:
    if not args.package:
        pkgs = _all_indexed(registry)
        if not pkgs:
            return 1
        print(cg.run_across_packages(pkgs, lambda p: cg.callees(p.abs_path, args.symbol, args.limit)))
        return 0
    pkg, rc = _require_indexed(registry, args.package)
    if rc:
        return rc
    print(cg.callees(pkg.abs_path, args.symbol, args.limit))
    return 0


def cmd_impact(registry: Registry, args: argparse.Namespace) -> int:
    if not args.package:
        pkgs = _all_indexed(registry)
        if not pkgs:
            return 1
        print(cg.run_across_packages(pkgs, lambda p: cg.impact(p.abs_path, args.symbol, args.depth)))
        return 0
    pkg, rc = _require_indexed(registry, args.package)
    if rc:
        return rc
    print(cg.impact(pkg.abs_path, args.symbol, args.depth))
    return 0


def cmd_files(registry: Registry, args: argparse.Namespace) -> int:
    if not args.package:
        pkgs = _all_indexed(registry)
        if not pkgs:
            return 1
        print(cg.run_across_packages(pkgs, lambda p: cg.files(p.abs_path, args.filter or None)))
        return 0
    pkg, rc = _require_indexed(registry, args.package)
    if rc:
        return rc
    print(cg.files(pkg.abs_path, args.filter or None))
    return 0


def cmd_read(registry: Registry, args: argparse.Namespace) -> int:
    pkg, rc = _require_indexed(registry, args.package)
    if rc:
        return rc
    full_path = pkg.abs_path / args.file
    try:
        full_path.resolve().relative_to(pkg.abs_path.resolve())
    except ValueError:
        print("Error: file path escapes the package directory.", file=sys.stderr)
        return 1
    if not full_path.exists():
        print(f"File not found: {args.file}", file=sys.stderr)
        return 1
    lines = full_path.read_text(encoding="utf-8", errors="replace").splitlines()
    total = len(lines)
    start = max(0, (args.start or 1) - 1)
    end = min(total, args.end) if args.end else total
    snippet = "\n".join(lines[start:end])
    range_str = f"lines {start + 1}–{end} of {total}" if (args.start or args.end) else f"{total} lines"
    print(f"# {args.package}/{args.file}  ({range_str})\n")
    print(snippet)
    return 0


# ---------------------------------------------------------------------------
# Argument parser
# ---------------------------------------------------------------------------

def main_cli() -> None:
    parser = argparse.ArgumentParser(
        prog="codegraph-hub",
        description="Manage and query the multi-repo codegraph index for GitHub Copilot.",
    )
    sub = parser.add_subparsers(dest="command", metavar="<command>")
    sub.required = True

    # ---- management --------------------------------------------------------

    sub.add_parser("list", help="List registered packages")

    p_add = sub.add_parser("add", help="Register a local repo and index it")
    p_add.add_argument("name", help="Short identifier, e.g. pandas")
    p_add.add_argument("path", help="Path to the locally cloned repo")
    p_add.add_argument("-d", "--description", help="One-line description of the package")

    p_rm = sub.add_parser("remove", help="Remove a package from the registry")
    p_rm.add_argument("name", help="Package name to remove")

    p_ri = sub.add_parser("reindex", help="Re-sync the codegraph index for a package")
    p_ri.add_argument("name", help="Package name to re-index")

    p_st = sub.add_parser("status", help="Show codegraph index stats for a package")
    p_st.add_argument("name", help="Package name")

    # ---- query -------------------------------------------------------------

    p_search = sub.add_parser("search", help="Search for symbols in a package (or all packages)")
    p_search.add_argument("package", nargs="?", default=None, help="Package name (omit to search all)")
    p_search.add_argument("query", help="Symbol name or keyword to search for")
    p_search.add_argument("-k", "--kind", help="Filter by kind: function, class, method, module, variable")
    p_search.add_argument("-n", "--limit", type=int, default=20, help="Max results (default: 20)")

    p_ctx = sub.add_parser("context", help="Get AI-optimized context for a task (in a package or all)")
    p_ctx.add_argument("package", nargs="?", default=None, help="Package name (omit to query all)")
    p_ctx.add_argument("task", nargs="+", help="Task description, e.g. 'how to write a test'")

    p_sym = sub.add_parser("symbol", help="Show the source of a named symbol (searches all packages if none given)")
    p_sym.add_argument("package", nargs="?", default=None, help="Package name (omit to search all, stops at first match)")
    p_sym.add_argument("symbol", help="Symbol name (function, class, method, etc.)")
    p_sym.add_argument("-k", "--kind", help="Filter by kind if name is ambiguous")
    p_sym.add_argument("-c", "--context", type=int, default=0, metavar="N",
                       help="Extra surrounding lines above/below (default: 0)")

    p_callers = sub.add_parser("callers", help="Find what calls a symbol (in a package or all)")
    p_callers.add_argument("package", nargs="?", default=None, help="Package name (omit to search all)")
    p_callers.add_argument("symbol", help="Symbol name")
    p_callers.add_argument("-n", "--limit", type=int, default=20, help="Max results (default: 20)")

    p_callees = sub.add_parser("callees", help="Find what a symbol calls (in a package or all)")
    p_callees.add_argument("package", nargs="?", default=None, help="Package name (omit to search all)")
    p_callees.add_argument("symbol", help="Symbol name")
    p_callees.add_argument("-n", "--limit", type=int, default=20, help="Max results (default: 20)")

    p_impact = sub.add_parser("impact", help="Analyze blast radius of changing a symbol (in a package or all)")
    p_impact.add_argument("package", nargs="?", default=None, help="Package name (omit to search all)")
    p_impact.add_argument("symbol", help="Symbol name")
    p_impact.add_argument("--depth", type=int, default=3, help="Trace depth (default: 3)")

    p_files = sub.add_parser("files", help="List file structure of a package (or all packages)")
    p_files.add_argument("package", nargs="?", default=None, help="Package name (omit to list all)")
    p_files.add_argument("filter", nargs="?", default="", help="Optional glob filter, e.g. '*.py'")

    p_read = sub.add_parser("read", help="Print a file (or line range) from a package")
    p_read.add_argument("package", help="Package name")
    p_read.add_argument("file", help="Relative file path, e.g. pandas/runner.py")
    p_read.add_argument("start", nargs="?", type=int, default=None, help="Start line (1-indexed)")
    p_read.add_argument("end", nargs="?", type=int, default=None, help="End line (inclusive)")

    # ---- serve -------------------------------------------------------------

    sub.add_parser("serve", help="Start the MCP server (used by VS Code / Copilot)")

    # ---- dispatch ----------------------------------------------------------

    args = parser.parse_args()

    if args.command == "serve":
        from .server import main as serve_main
        serve_main()
        return

    # Merge multi-word task into a single string
    if args.command == "context":
        args.task = " ".join(args.task)

    registry = Registry()
    dispatch = {
        "list":     cmd_list,
        "add":      cmd_add,
        "remove":   cmd_remove,
        "reindex":  cmd_reindex,
        "status":   cmd_status,
        "search":   cmd_search,
        "context":  cmd_context,
        "symbol":   cmd_symbol,
        "callers":  cmd_callers,
        "callees":  cmd_callees,
        "impact":   cmd_impact,
        "files":    cmd_files,
        "read":     cmd_read,
    }
    sys.exit(dispatch[args.command](registry, args))

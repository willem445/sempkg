"""Command-line interface for managing the codegraph-hub package registry."""

import argparse
import json
import sys
import urllib.request
from pathlib import Path

from . import codegraph as cg
from . import lance as lance_mod
from .bundle_store import (
    BundleInstallError,
    BundlePackage,
    VersionExistsError,
    get_all_bundle_packages,
    get_global_store,
    get_workspace_store,
)
from .registry import Registry


# ---------------------------------------------------------------------------
# Package management commands
# ---------------------------------------------------------------------------

def cmd_list(registry: Registry, args: argparse.Namespace) -> int:
    packages = registry.list_all()
    bundle_pkgs = get_all_bundle_packages()

    if not packages and not bundle_pkgs:
        print("No packages or bundles registered.")
        print("  Packages: 'codegraph-hub add <name> <path>'")
        print("  Bundles:  'codegraph-hub bundle sync' or 'codegraph-hub bundle add <pkg>@<ver>'")
        return 0

    if packages:
        print("Registered packages:")
        for pkg in packages:
            status = "indexed" if pkg.is_indexed else "NOT indexed"
            desc = f"  # {pkg.description}" if pkg.description else ""
            print(f"  {pkg.name:<24} [{status}]  {pkg.path}{desc}")

    if bundle_pkgs:
        if packages:
            print()
        print("Installed bundles:")
        for bp in bundle_pkgs:
            status = "indexed" if bp.is_indexed else "NOT indexed"
            lance_flag = "  +lance" if bp.has_lance() else ""
            scope = "workspace" if (Path(bp.path).parents[2].name == ".codegraph_hub") else "global"
            print(f"  {bp.name:<20} @ {bp.version:<12} [{status}]  [{scope}]{lance_flag}")
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
    """Return (pkg_or_bundle, error_code). Checks registry first, then bundle stores."""
    pkg = registry.get(name)
    if pkg is None:
        # Check installed bundles (workspace-first precedence)
        for bp in get_all_bundle_packages():
            if bp.name == name:
                if not bp.is_indexed:
                    print(
                        f"Bundle '{name}@{bp.version}' is installed but not queryable "
                        "(missing graph/ directory).",
                        file=sys.stderr,
                    )
                    return None, 1
                return bp, 0
        print(
            f"Package '{name}' not found. "
            "Use 'codegraph-hub list' to see registered packages and bundles.",
            file=sys.stderr,
        )
        return None, 1
    if not pkg.is_indexed:
        print(f"Package '{name}' is not indexed. Run 'codegraph-hub reindex {name}' first.", file=sys.stderr)
        return None, 1
    return pkg, 0


def _all_queryable(registry: Registry) -> list:
    """Return all queryable sources: indexed registry packages + indexed bundles."""
    reg_pkgs = [p for p in registry.list_all() if p.is_indexed]
    bundle_pkgs = [b for b in get_all_bundle_packages() if b.is_indexed]
    combined = reg_pkgs + bundle_pkgs
    if not combined:
        print(
            "No indexed packages or bundles found.\n"
            "  Packages: 'codegraph-hub add <name> <path>'\n"
            "  Bundles:  'codegraph-hub bundle sync' or "
            "'codegraph-hub bundle add <pkg>@<ver>'",
            file=sys.stderr,
        )
    return combined


def _all_indexed(registry: Registry) -> list:
    """Legacy alias — delegates to _all_queryable."""
    return _all_queryable(registry)


def cmd_search(registry: Registry, args: argparse.Namespace) -> int:
    kind = getattr(args, "kind", None) or None
    if not args.package:
        pkgs = _all_queryable(registry)
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
        pkgs = _all_queryable(registry)
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
        pkgs = _all_queryable(registry)
        if not pkgs:
            return 1
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
    """Return formatted symbol source. For bundles (no source files), falls back to CLI search."""
    if isinstance(pkg, BundlePackage):
        # Bundles carry the graph index but not source files; use CLI search as best-effort.
        return cg.query(pkg.abs_path, symbol, kind, limit=5)
    matches = cg.find_symbols(pkg.abs_path, symbol, kind)
    if not matches:
        return ""
    return "\n\n---\n\n".join(cg.read_symbol_source(pkg.abs_path, m, context_lines) for m in matches)


def cmd_callers(registry: Registry, args: argparse.Namespace) -> int:
    if not args.package:
        pkgs = _all_queryable(registry)
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
        pkgs = _all_queryable(registry)
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
        pkgs = _all_queryable(registry)
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
        pkgs = _all_queryable(registry)
        if not pkgs:
            return 1
        print(cg.run_across_packages(pkgs, lambda p: cg.files(p.abs_path, args.filter or None)))
        return 0
    pkg, rc = _require_indexed(registry, args.package)
    if rc:
        return rc
    print(cg.files(pkg.abs_path, args.filter or None))
    return 0


# ---------------------------------------------------------------------------
# Bundle commands
# ---------------------------------------------------------------------------

def _parse_pkg_version(spec: str) -> tuple[str, str]:
    """Parse 'name@version' format. Raises SystemExit on bad format."""
    if "@" not in spec:
        print(
            f"Error: expected '<pkg>@<version>', got '{spec}'",
            file=sys.stderr,
        )
        sys.exit(1)
    name, _, version = spec.partition("@")
    if not name or not version:
        print(
            f"Error: expected '<pkg>@<version>', got '{spec}'",
            file=sys.stderr,
        )
        sys.exit(1)
    return name, version


def cmd_bundle_install(args: argparse.Namespace) -> int:
    pkg, version = _parse_pkg_version(args.pkg_version)
    store = get_global_store() if args.glob else get_workspace_store()
    verify_key_path = Path(args.verify_key) if args.verify_key else None
    try:
        info = store.install_from_registry(pkg, version, args.registry, verify_key_path=verify_key_path)
    except VersionExistsError as exc:
        print(f"Error: {exc}", file=sys.stderr)
        return 1
    except BundleInstallError as exc:
        print(f"Error: {exc}", file=sys.stderr)
        return 1
    print(f"Installed {info.name}@{info.version} to {info.store_dir}")
    return 0


def cmd_bundle_list(args: argparse.Namespace) -> int:
    show_global = args.glob
    show_workspace = args.workspace
    if not show_global and not show_workspace:
        show_global = True
        show_workspace = True

    entries: list[tuple[str, str, str]] = []
    if show_workspace:
        ws_store = get_workspace_store()
        for info in ws_store.list_installed():
            entries.append((f"{info.name}@{info.version}", "workspace", str(info.store_dir)))
    if show_global:
        g_store = get_global_store()
        for info in g_store.list_installed():
            entries.append((f"{info.name}@{info.version}", "global", str(info.store_dir)))

    if not entries:
        print("No bundles installed.")
        return 0
    for spec, scope, path in entries:
        print(f"  {spec:<30}  [{scope}]  {path}")
    return 0


def cmd_bundle_remove(args: argparse.Namespace) -> int:
    pkg, version = _parse_pkg_version(args.pkg_version)
    store = get_global_store() if args.glob else get_workspace_store()
    if store.remove(pkg, version):
        print(f"Removed {pkg}@{version}")
        return 0
    print(f"Not found: {pkg}@{version}", file=sys.stderr)
    return 1


def cmd_bundle_add(args: argparse.Namespace) -> int:
    from .workspace_manifest import (
        BundleDep,
        LockFile,
        RegistryConfig,
        WorkspaceManifest,
        MANIFEST_FILENAME,
        LOCK_FILENAME,
        load_manifest,
        save_manifest,
        load_lock,
        save_lock,
        resolve_lock,
    )

    pkg, version = _parse_pkg_version(args.pkg_version)
    cwd = Path.cwd()

    # Load or create manifest
    try:
        manifest = load_manifest(cwd)
    except FileNotFoundError:
        manifest = WorkspaceManifest(registries=[], dependencies={}, verify_key=None)

    registry_name: str | None = args.registry
    registry_url: str | None = args.registry_url

    if registry_url:
        registry_url = registry_url.rstrip("/")
        existing = next((r for r in manifest.registries if r.url == registry_url), None)
        if existing:
            registry_name = existing.name
        else:
            if not registry_name:
                if not manifest.registries:
                    registry_name = "default"
                else:
                    registry_name = f"registry-{len(manifest.registries)}"
            manifest.registries.append(RegistryConfig(name=registry_name, url=registry_url))
    elif registry_name:
        if manifest.get_registry(registry_name) is None:
            print(
                f"Error: registry '{registry_name}' not found in {MANIFEST_FILENAME}. "
                "Use --registry-url to add it.",
                file=sys.stderr,
            )
            return 1
    else:
        default_reg = manifest.default_registry()
        if default_reg is None:
            print(
                "Error: no registry specified and no registries defined. "
                "Use --registry-url <url> to specify a registry.",
                file=sys.stderr,
            )
            return 1
        registry_name = default_reg.name

    manifest.dependencies[pkg] = BundleDep(name=pkg, version=version, registry=registry_name)
    save_manifest(manifest, cwd)

    # Update lock
    lock = load_lock(cwd)
    try:
        fresh_lock = resolve_lock(manifest, cwd)
    except Exception as exc:  # noqa: BLE001
        print(f"Error: could not resolve lock from registry: {exc}", file=sys.stderr)
        return 1
    for name, entry in fresh_lock.packages.items():
        lock.packages[name] = entry
    save_lock(lock, cwd)

    # Install
    lock_entry = lock.packages.get(pkg)
    if lock_entry is None:
        print(f"Error: could not resolve lock entry for {pkg}@{version}", file=sys.stderr)
        return 1

    store = get_workspace_store(cwd)
    try:
        info = store.install_from_registry(pkg, version, lock_entry.registry_url)
    except VersionExistsError:
        print(
            f"Added {pkg}@{version} (registry: {registry_name}) "
            f"\u2014 already installed at {store.get_bundle_dir(pkg, version)}"
        )
        return 0
    except BundleInstallError as exc:
        print(f"Error: {exc}", file=sys.stderr)
        return 1

    print(f"Added {pkg}@{version} (registry: {registry_name}) \u2014 installed to {info.store_dir}")
    return 0


def cmd_bundle_sync(args: argparse.Namespace) -> int:
    from .workspace_manifest import sync_workspace

    cwd = Path.cwd()
    verify_key_path = Path(args.verify_key) if args.verify_key else None
    try:
        installed = sync_workspace(cwd, verify_key_path=verify_key_path, reinstall=args.reinstall)
    except FileNotFoundError as exc:
        print(f"Error: {exc}", file=sys.stderr)
        return 1
    except Exception as exc:  # noqa: BLE001
        print(f"Error: {exc}", file=sys.stderr)
        return 1

    if not installed:
        print("All bundles already installed.")
    else:
        for spec in installed:
            print(f"  Installed {spec}")
    print(f"Synced {len(installed)} bundle(s).")
    return 0


def cmd_bundle_lock(args: argparse.Namespace) -> int:
    from .workspace_manifest import (
        MANIFEST_FILENAME,
        LOCK_FILENAME,
        load_manifest,
        resolve_lock,
        save_lock,
    )

    cwd = Path.cwd()
    try:
        manifest = load_manifest(cwd)
    except FileNotFoundError:
        print(
            f"Error: {MANIFEST_FILENAME} not found. "
            "Run 'codegraph-hub bundle add' to create one.",
            file=sys.stderr,
        )
        return 1

    try:
        lock = resolve_lock(manifest, cwd)
    except Exception as exc:  # noqa: BLE001
        print(f"Error: {exc}", file=sys.stderr)
        return 1

    save_lock(lock, cwd)
    print(f"Lock file updated: {LOCK_FILENAME} ({len(lock.packages)} package(s))")
    return 0


def cmd_bundle_search_registry(args: argparse.Namespace) -> int:
    url = args.url.rstrip("/") + "/index.json"
    try:
        with urllib.request.urlopen(url) as resp:  # noqa: S310
            data = json.loads(resp.read().decode("utf-8"))
    except Exception as exc:  # noqa: BLE001
        print(f"Error fetching {url}: {exc}", file=sys.stderr)
        return 1
    packages: dict = data.get("packages", {})
    if not packages:
        print("Registry is empty.")
        return 0
    for name, info in sorted(packages.items()):
        versions = ", ".join(info.get("versions", []))
        latest = info.get("latest", "")
        print(f"  {name:<30}  versions: {versions}  (latest: {latest})")
    return 0


def cmd_bundle_lance_search(args: argparse.Namespace) -> int:
    """Search the LanceDB documentation index of one or all installed bundles."""
    query = args.query
    limit = args.limit

    if args.pkg_version:
        name, version = _parse_pkg_version(args.pkg_version)
        ws_store = get_workspace_store()
        bundle_dir = ws_store.resolve(name, version)
        if bundle_dir is None:
            bundle_dir = get_global_store().resolve(name, version)
        if bundle_dir is None:
            print(f"Bundle '{name}@{version}' is not installed.", file=sys.stderr)
            return 1
        if not lance_mod.has_lance(bundle_dir):
            print(f"Bundle '{name}@{version}' does not have a LanceDB documentation index.", file=sys.stderr)
            return 1
        print(lance_mod.lance_search(bundle_dir, query, limit))
        return 0

    # No specific bundle — search all installed bundles with Lance indexes
    bundle_pkgs = get_all_bundle_packages()
    lance_bundles = [bp for bp in bundle_pkgs if lance_mod.has_lance(bp.abs_path)]
    if not lance_bundles:
        print("No installed bundles have a LanceDB documentation index.", file=sys.stderr)
        return 1

    found_any = False
    for bp in lance_bundles:
        result = lance_mod.lance_search(bp.abs_path, query, limit)
        if result and not result.startswith("(no results"):
            if found_any:
                print("\n" + "─" * 60 + "\n")
            print(f"### {bp.name} @ {bp.version}\n")
            print(result)
            found_any = True

    if not found_any:
        print(f"No LanceDB documentation results for '{query}' across installed bundles.")
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

    # ---- bundle ------------------------------------------------------------

    p_bundle = sub.add_parser("bundle", help="Manage .cgbundle archives")
    bundle_sub = p_bundle.add_subparsers(dest="bundle_command", metavar="<bundle-command>")
    bundle_sub.required = True

    pb_install = bundle_sub.add_parser("install", help="Install a bundle from a registry")
    pb_install.add_argument("pkg_version", metavar="<pkg>@<version>", help="Package and version, e.g. mylib@1.0.0")
    pb_install.add_argument("--registry", required=True, metavar="URL", help="Registry base URL")
    pb_install.add_argument("--global", dest="glob", action="store_true", help="Install to global store")
    pb_install.add_argument("--verify-key", metavar="PATH", default=None, help="Path to Ed25519 public key PEM file for signature verification")

    pb_list = bundle_sub.add_parser("list", help="List installed bundles")
    pb_list.add_argument("--global", dest="glob", action="store_true", help="Show only global store")
    pb_list.add_argument("--workspace", action="store_true", help="Show only workspace store")

    pb_remove = bundle_sub.add_parser("remove", help="Remove an installed bundle")
    pb_remove.add_argument("pkg_version", metavar="<pkg>@<version>", help="Package and version, e.g. mylib@1.0.0")
    pb_remove.add_argument("--global", dest="glob", action="store_true", help="Remove from global store")

    pb_search = bundle_sub.add_parser("search-registry", help="List packages available on a registry")
    pb_search.add_argument("url", metavar="URL", help="Registry base URL")

    pb_lance = bundle_sub.add_parser("lance-search", help="Full-text search the LanceDB documentation index of a bundle")
    pb_lance.add_argument("pkg_version", metavar="<pkg>@<version>", nargs="?", default=None,
                          help="Bundle name@version (omit to search all bundles with Lance)")
    pb_lance.add_argument("query", help="Documentation search query")
    pb_lance.add_argument("-n", "--limit", type=int, default=10, help="Max results (default: 10)")

    pb_add = bundle_sub.add_parser("add", help="Add a bundle dependency and install it")
    pb_add.add_argument("pkg_version", metavar="<pkg>@<version>")
    pb_add.add_argument("--registry", metavar="NAME", default=None, help="Registry name from codegraph-hub.toml")
    pb_add.add_argument("--registry-url", metavar="URL", default=None, help="Registry base URL (adds to manifest if new)")

    pb_sync = bundle_sub.add_parser("sync", help="Install all bundles from codegraph-hub.toml")
    pb_sync.add_argument("--verify-key", metavar="PATH", default=None)
    pb_sync.add_argument("--reinstall", action="store_true", help="Reinstall even if already installed")

    bundle_sub.add_parser("lock", help="Refresh codegraph-hub.lock from registries without installing")

    # ---- serve -------------------------------------------------------------

    sub.add_parser("serve", help="Start the MCP server (used by VS Code / Copilot)")

    # ---- dispatch ----------------------------------------------------------

    args = parser.parse_args()

    if args.command == "serve":
        from .server import main as serve_main
        serve_main()
        return

    if args.command == "bundle":
        bundle_dispatch = {
            "install":         cmd_bundle_install,
            "list":            cmd_bundle_list,
            "remove":          cmd_bundle_remove,
            "search-registry": cmd_bundle_search_registry,
            "add":             cmd_bundle_add,
            "sync":            cmd_bundle_sync,
            "lock":            cmd_bundle_lock,
            "lance-search":    cmd_bundle_lance_search,
        }
        sys.exit(bundle_dispatch[args.bundle_command](args))

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

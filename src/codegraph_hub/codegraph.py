"""Thin wrapper around the codegraph CLI and SQLite database."""

import shutil
import sqlite3
import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import Optional


# ---------------------------------------------------------------------------
# Resolve codegraph executable once at import time
# On Windows, Node global tools install as .cmd wrappers which subprocess
# can't find without shell=True.  shutil.which() resolves the full path
# (e.g. codegraph.cmd) so we can call it directly without shell=True.
# ---------------------------------------------------------------------------

def _find_codegraph() -> str:
    exe = shutil.which("codegraph")
    if exe is None:
        raise FileNotFoundError(
            "codegraph executable not found on PATH.\n"
            "Install it with: npm install -g @colbymchenry/codegraph\n"
            "or: irm https://raw.githubusercontent.com/colbymchenry/codegraph/main/install.ps1 | iex"
        )
    return exe

_CODEGRAPH = _find_codegraph()


# ---------------------------------------------------------------------------
# SQLite helpers — read directly from .codegraph/codegraph.db
# ---------------------------------------------------------------------------

def _db_path(project_path: Path) -> Path:
    return project_path / ".codegraph" / "codegraph.db"


@dataclass
class SymbolLocation:
    name: str
    qualified_name: str
    kind: str
    file_path: str        # relative to project root as stored in DB
    start_line: int
    end_line: int
    signature: Optional[str]
    docstring: Optional[str]
    language: str


def find_symbols(project_path: Path, name: str, kind: Optional[str] = None, limit: int = 10) -> list[SymbolLocation]:
    """Query the codegraph SQLite DB for symbols matching name."""
    db = _db_path(project_path)
    if not db.exists():
        return []
    with sqlite3.connect(f"file:{db}?mode=ro", uri=True) as conn:
        conn.row_factory = sqlite3.Row
        if kind:
            rows = conn.execute(
                "SELECT name, qualified_name, kind, file_path, start_line, end_line, "
                "signature, docstring, language FROM nodes "
                "WHERE lower(name) = lower(?) AND kind = ? LIMIT ?",
                (name, kind, limit),
            ).fetchall()
        else:
            rows = conn.execute(
                "SELECT name, qualified_name, kind, file_path, start_line, end_line, "
                "signature, docstring, language FROM nodes "
                "WHERE lower(name) = lower(?) LIMIT ?",
                (name, limit),
            ).fetchall()
    return [SymbolLocation(**dict(r)) for r in rows]


def read_symbol_source(project_path: Path, symbol: SymbolLocation, context_lines: int = 0) -> str:
    """Read the source lines for a symbol from disk, with optional surrounding context."""
    # The DB stores file_path — may be absolute or relative; normalise.
    fpath = Path(symbol.file_path)
    if not fpath.is_absolute():
        fpath = project_path / fpath

    if not fpath.exists():
        return f"(file not found: {symbol.file_path})"

    lines = fpath.read_text(encoding="utf-8", errors="replace").splitlines()
    start = max(0, symbol.start_line - 1 - context_lines)   # DB lines are 1-indexed
    end = min(len(lines), symbol.end_line + context_lines)
    snippet = "\n".join(lines[start:end])

    header = f"# {symbol.qualified_name}  [{symbol.kind}]  {symbol.file_path}:{symbol.start_line}-{symbol.end_line}"
    lang = Path(symbol.file_path).suffix.lstrip(".") or "text"
    return f"{header}\n\n```{lang}\n{snippet}\n```"


# ---------------------------------------------------------------------------
# CLI wrappers
# ---------------------------------------------------------------------------

def _run(args: list[str], cwd: Optional[Path] = None, timeout: int = 60) -> tuple[int, str, str]:
    result = subprocess.run(
        [_CODEGRAPH] + args,
        cwd=str(cwd) if cwd else None,
        capture_output=True,
        timeout=timeout,
        # codegraph outputs UTF-8 (box-drawing chars, emoji); force it
        # rather than letting Windows default to cp1252
        encoding="utf-8",
        errors="replace",
    )
    return result.returncode, (result.stdout or "").strip(), (result.stderr or "").strip()


def init_and_index(path: Path) -> str:
    """Run codegraph init --index on a project directory."""
    rc, out, err = _run(["init", "--index", str(path)], timeout=300)
    if rc != 0:
        raise RuntimeError(err or out or "codegraph init failed with no output")
    return out or "Indexing complete."


def sync(path: Path) -> str:
    """Incrementally sync an existing index."""
    rc, out, err = _run(["sync", str(path)], timeout=120)
    if rc != 0:
        raise RuntimeError(err or out or "codegraph sync failed")
    return out or "Sync complete."


def status(path: Path) -> str:
    rc, out, err = _run(["status", str(path)])
    return out or err


def query(path: Path, search: str, kind: Optional[str] = None, limit: int = 20) -> str:
    args = ["query", search, "--json", f"--limit={limit}"]
    if kind:
        args.append(f"--kind={kind}")
    rc, out, err = _run(args, cwd=path)
    return out or err or "(no results)"


def callers(path: Path, symbol: str, limit: int = 20) -> str:
    rc, out, err = _run(["callers", symbol, "--json", f"--limit={limit}"], cwd=path)
    return out or err or "(no results)"


def callees(path: Path, symbol: str, limit: int = 20) -> str:
    rc, out, err = _run(["callees", symbol, "--json", f"--limit={limit}"], cwd=path)
    return out or err or "(no results)"


def context(path: Path, task: str) -> str:
    rc, out, err = _run(["context", task], cwd=path)
    return out or err or "(no results)"


def impact(path: Path, symbol: str, depth: int = 3) -> str:
    rc, out, err = _run(["impact", symbol, "--json", f"--depth={depth}"], cwd=path)
    return out or err or "(no results)"


def files(path: Path, filter_str: Optional[str] = None) -> str:
    args = ["files", "--json"]
    if filter_str:
        args += ["--filter", filter_str]
    rc, out, err = _run(args, cwd=path)
    return out or err or "(no results)"


# ---------------------------------------------------------------------------
# Cross-package aggregation
# ---------------------------------------------------------------------------

from typing import Callable, TYPE_CHECKING
if TYPE_CHECKING:
    from .registry import Package


def run_across_packages(
    packages: "list[Package]",
    fn: "Callable[[Package], str]",
    stop_on_first: bool = False,
) -> str:
    """Run fn(pkg) over every indexed package and aggregate the output.

    Args:
        packages: list of Package objects to iterate.
        fn: callable that takes a Package and returns a result string.
        stop_on_first: if True, stop after the first package that returns
                       non-empty results (useful for symbol lookup).
    """
    if not packages:
        return "No indexed packages registered."

    sections: list[str] = []
    for pkg in packages:
        if not pkg.is_indexed:
            continue
        try:
            result = fn(pkg)
        except Exception as exc:
            result = f"(error: {exc})"

        if result and result not in ("(no results)", "[]", ""):
            sections.append(f"### {pkg.name}\n\n{result}")
            if stop_on_first:
                break

    if not sections:
        return "(no results in any package)"
    return "\n\n".join(sections)

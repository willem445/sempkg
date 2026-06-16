"""LanceDB documentation index search for cgbundle packages."""

from __future__ import annotations

import json
from pathlib import Path


def lance_dir_path(bundle_dir: Path) -> Path:
    """Return the path to the LanceDB directory inside a bundle directory."""
    return bundle_dir / "lance"


def has_lance(bundle_dir: Path) -> bool:
    """Return True if the bundle contains a LanceDB documentation index."""
    return lance_dir_path(bundle_dir).is_dir()


def lance_metadata(bundle_dir: Path) -> dict:
    """Load lance/metadata.json from a bundle directory. Returns {} if absent."""
    meta_path = bundle_dir / "lance" / "metadata.json"
    if not meta_path.exists():
        return {}
    try:
        return json.loads(meta_path.read_text(encoding="utf-8"))
    except (json.JSONDecodeError, OSError):
        return {}


def lance_search(bundle_dir: Path, query: str, limit: int = 10) -> str:
    """Full-text (BM25) search the LanceDB documentation index inside a bundle.

    Args:
        bundle_dir: Path to the extracted bundle store directory.
        query:      Search query string.
        limit:      Maximum number of results to return (default 10).

    Returns:
        A formatted markdown string with matching document excerpts, or a
        human-readable "(no results)" / "(error …)" message.
    """
    lance_dir = lance_dir_path(bundle_dir)
    if not lance_dir.is_dir():
        return "(no LanceDB documentation index in this bundle)"

    try:
        import lancedb  # noqa: PLC0415
    except ImportError:
        return "(lancedb package not installed — run: uv pip install lancedb)"

    try:
        db = lancedb.connect(str(lance_dir))
        tbl = db.open_table("docs")
        results = tbl.search(query, query_type="fts").limit(limit).to_list()
    except Exception as exc:  # noqa: BLE001
        return f"(error searching LanceDB index: {exc})"

    if not results:
        return f"(no results for '{query}')"

    parts = [
        f"**{row.get('path', '?')}**\n{str(row.get('content', ''))[:400]}"
        for row in results
    ]
    return "\n\n---\n\n".join(parts)

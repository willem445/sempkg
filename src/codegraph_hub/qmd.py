"""QMD documentation index search for cgbundle packages."""

from __future__ import annotations

import json
import sqlite3
from pathlib import Path


def qmd_db_path(bundle_dir: Path) -> Path:
    """Return the path to the QMD SQLite database inside a bundle directory."""
    return bundle_dir / "qmd" / "index" / "index.sqlite"


def has_qmd(bundle_dir: Path) -> bool:
    """Return True if the bundle contains a QMD documentation index."""
    return qmd_db_path(bundle_dir).exists()


def qmd_metadata(bundle_dir: Path) -> dict:
    """Load qmd/metadata.json from a bundle directory. Returns {} if absent."""
    meta_path = bundle_dir / "qmd" / "metadata.json"
    if not meta_path.exists():
        return {}
    try:
        return json.loads(meta_path.read_text(encoding="utf-8"))
    except (json.JSONDecodeError, OSError):
        return {}


def qmd_search(bundle_dir: Path, query: str, limit: int = 10) -> str:
    """Full-text search the QMD documentation index inside a bundle.

    Uses the FTS5 ``documents_fts`` table for BM25-ranked search, falling back
    to a simple LIKE scan if the FTS5 table is not present.

    Args:
        bundle_dir: Path to the extracted bundle store directory.
        query:      Search query string.
        limit:      Maximum number of results to return (default 10).

    Returns:
        A formatted markdown string with matching document excerpts, or a
        human-readable "(no results)" / "(error …)" message.
    """
    db = qmd_db_path(bundle_dir)
    if not db.exists():
        return "(no QMD documentation index in this bundle)"

    try:
        with sqlite3.connect(f"file:{db}?mode=ro", uri=True) as conn:
            conn.row_factory = sqlite3.Row

            # Try FTS5 full-text search first (preferred — BM25 ranked)
            try:
                rows = conn.execute(
                    "SELECT d.path, "
                    "snippet(documents_fts, -1, '**', '**', '...', 64) AS snip "
                    "FROM documents AS d "
                    "JOIN documents_fts ON d.rowid = documents_fts.rowid "
                    "WHERE documents_fts MATCH ? "
                    "ORDER BY rank LIMIT ?",
                    (query, limit),
                ).fetchall()
            except sqlite3.OperationalError:
                # Fallback: plain LIKE scan if FTS5 table / column names differ
                try:
                    rows = conn.execute(
                        "SELECT path, substr(content, 1, 400) AS snip "
                        "FROM documents "
                        "WHERE content LIKE ? LIMIT ?",
                        (f"%{query}%", limit),
                    ).fetchall()
                except sqlite3.OperationalError as exc2:
                    return f"(error querying QMD index: {exc2})"

    except sqlite3.Error as exc:
        return f"(error opening QMD index: {exc})"

    if not rows:
        return f"(no results for '{query}')"

    parts = [f"**{row['path']}**\n{row['snip']}" for row in rows]
    return "\n\n---\n\n".join(parts)

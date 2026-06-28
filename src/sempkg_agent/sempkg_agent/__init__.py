"""sempkg-agent: a grounded code-intelligence agent server over sempkg bundles.

The agent receives a natural-language request from a calling agent (over A2A,
MCP, or REST), performs version-accurate retrieval against installed sembundles
via the local ``sempkg`` MCP server, and returns exactly the context the caller
needs: which package it came from, the files and line ranges, full snippets, the
reasoning behind the selection, and a summary.
"""

from __future__ import annotations

__version__ = "0.1.0"

__all__ = ["__version__"]

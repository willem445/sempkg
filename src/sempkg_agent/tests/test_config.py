"""Config tests (no network)."""

from __future__ import annotations

from sempkg_agent.config import MCPSettings


def test_argv_includes_workspace() -> None:
    s = MCPSettings(workspace="/ws")
    assert s.argv() == ["mcp", "-C", "/ws"]


def test_argv_appends_extra_args() -> None:
    s = MCPSettings(workspace=".", extra_args="--verbose --foo bar")
    assert s.argv() == ["mcp", "-C", ".", "--verbose", "--foo", "bar"]


def test_allowed_tools_csv_split() -> None:
    s = MCPSettings(allowed_tools="query, read_code ,read_symbol")
    assert s.allowed_tools == ["query", "read_code", "read_symbol"]


def test_allowed_tools_default_is_query_first() -> None:
    s = MCPSettings()
    assert s.allowed_tools[0] == "query"
    assert "read_code" in s.allowed_tools

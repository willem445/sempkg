"""Config tests (no network)."""

from __future__ import annotations

import pytest

from sempkg_agent.config import AgentSettings, MCPSettings


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


def test_default_mode_is_human() -> None:
    assert AgentSettings().mode == "human"


def test_system_prompt_file_is_loaded(tmp_path) -> None:
    p = tmp_path / "prompt.txt"
    p.write_text("custom behaviour", encoding="utf-8")
    s = AgentSettings(system_prompt_file=str(p))
    assert s.system_prompt == "custom behaviour"


def test_missing_system_prompt_file_raises() -> None:
    with pytest.raises(ValueError):
        AgentSettings(system_prompt_file="/no/such/prompt/file.txt")

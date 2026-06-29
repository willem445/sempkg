"""Prompt selection + message-building tests (no network)."""

from __future__ import annotations

from sempkg_agent.prompts import SYSTEM_PROMPT, build_user_message, system_prompt_for


def test_human_mode_prompt_is_audience_aware() -> None:
    p = system_prompt_for("human", "Acme Knowledge")
    assert "Acme Knowledge" in p
    assert "calling agent" not in p.lower()  # human persona, not agent-to-agent
    assert "I don't know" in p  # grounding discipline
    assert "latest" in p.lower()  # version policy


def test_agent_mode_returns_machine_prompt() -> None:
    assert system_prompt_for("agent", "X") is SYSTEM_PROMPT


def test_custom_prompt_overrides_persona() -> None:
    custom = "You are a pirate. Answer only in nautical terms."
    assert system_prompt_for("human", "X", custom) == custom
    assert system_prompt_for("agent", "X", custom) == custom


def test_build_user_message_defaults_to_latest_version() -> None:
    msg = build_user_message("how does X work?", None, None)
    assert "LATEST" in msg


def test_build_user_message_scopes_to_named_release() -> None:
    msg = build_user_message("how did X work?", None, "v14.2.0")
    assert "v14.2.0" in msg
    assert "Do not blend" in msg

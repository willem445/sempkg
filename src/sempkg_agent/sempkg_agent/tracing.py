"""Opt-in execution tracing for the agent loop.

When enabled (``SEMPKG_AGENT_TRACE=1``), an ``AgentTracer`` callback logs, to the
``sempkg_agent.trace`` logger (stderr), every step the agent takes:

* the LLM's reasoning text for each turn,
* each tool the LLM decided to call (name + arguments),
* each sempkg tool invocation's input and (truncated) output.

This is the cheap, self-contained way to verify the agent is querying sensibly
without sending data to an external service. For a richer hosted UI, set the
standard LangSmith env vars instead (``LANGCHAIN_TRACING_V2=true`` +
``LANGCHAIN_API_KEY``) — note that routes traces to LangSmith's cloud.
"""

from __future__ import annotations

import json
import logging
from typing import Any

from langchain_core.callbacks import AsyncCallbackHandler

trace_logger = logging.getLogger("sempkg_agent.trace")


def _trunc(text: str, limit: int) -> str:
    text = text.replace("\n", " ⏎ ")
    if len(text) <= limit:
        return text
    return f"{text[:limit]}… (+{len(text) - limit} more chars)"


class AgentTracer(AsyncCallbackHandler):
    """Logs LLM reasoning + tool I/O for one agent run."""

    def __init__(self, max_chars: int = 1200) -> None:
        self._max = max_chars
        self._tool_names: dict[Any, str] = {}

    async def on_llm_end(self, response: Any, **kwargs: Any) -> None:
        try:
            gen = response.generations[0][0]
        except (AttributeError, IndexError):
            return
        message = getattr(gen, "message", None)
        text = (getattr(message, "content", None) or getattr(gen, "text", "") or "").strip()
        tool_calls = getattr(message, "tool_calls", None) or []
        if text:
            trace_logger.info("LLM reasoning: %s", _trunc(text, self._max))
        for tc in tool_calls:
            args = json.dumps(tc.get("args", {}), default=str)
            trace_logger.info("LLM → tool call: %s(%s)", tc.get("name"), _trunc(args, 500))
        if not text and not tool_calls:
            trace_logger.info("LLM → (final structured answer)")

    async def on_tool_start(
        self, serialized: dict, input_str: str, **kwargs: Any
    ) -> None:
        name = (serialized or {}).get("name", "tool")
        self._tool_names[kwargs.get("run_id")] = name
        trace_logger.info("sempkg tool ► %s  input=%s", name, _trunc(str(input_str), 500))

    async def on_tool_end(self, output: Any, **kwargs: Any) -> None:
        name = self._tool_names.pop(kwargs.get("run_id"), "tool")
        trace_logger.info("sempkg tool ◄ %s  output=%s", name, _trunc(str(output), self._max))

    async def on_tool_error(self, error: BaseException, **kwargs: Any) -> None:
        name = self._tool_names.pop(kwargs.get("run_id"), "tool")
        trace_logger.warning("sempkg tool ✗ %s  error=%s", name, error)

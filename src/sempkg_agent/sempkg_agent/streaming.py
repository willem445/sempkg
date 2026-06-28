"""Queue-backed callback that turns the agent loop into a live event stream.

Used by ``KnowledgeAgent.astream`` to feed the chat UI real-time feedback: the
model's reasoning, each sempkg tool call as it starts, and each tool result as it
returns — the "watch the agent work" experience.
"""

from __future__ import annotations

import asyncio
from typing import Any

from langchain_core.callbacks import AsyncCallbackHandler

from .tracing import _trunc, is_final_answer_json


class StreamCallback(AsyncCallbackHandler):
    """Pushes structured step events onto an asyncio queue."""

    def __init__(self, queue: asyncio.Queue, max_chars: int = 2000) -> None:
        self._q = queue
        self._max = max_chars
        self._names: dict[Any, str] = {}

    async def on_llm_end(self, response: Any, **kwargs: Any) -> None:
        try:
            gen = response.generations[0][0]
        except (AttributeError, IndexError):
            return
        message = getattr(gen, "message", None)
        text = (getattr(message, "content", None) or getattr(gen, "text", "") or "").strip()
        if text and not is_final_answer_json(text):
            await self._q.put({"type": "reasoning", "text": _trunc(text, self._max)})

    async def on_tool_start(self, serialized: dict, input_str: str, **kwargs: Any) -> None:
        name = (serialized or {}).get("name", "tool")
        self._names[kwargs.get("run_id")] = name
        await self._q.put({"type": "tool_call", "name": name, "input": _trunc(str(input_str), 600)})

    async def on_tool_end(self, output: Any, **kwargs: Any) -> None:
        name = self._names.pop(kwargs.get("run_id"), "tool")
        await self._q.put(
            {"type": "tool_result", "name": name, "output": _trunc(str(output), self._max)}
        )

    async def on_tool_error(self, error: BaseException, **kwargs: Any) -> None:
        name = self._names.pop(kwargs.get("run_id"), "tool")
        await self._q.put({"type": "tool_error", "name": name, "error": str(error)})

"""MCP-mount transport: expose the agent itself as a network MCP server.

This lets MCP-native hosts mount sempkg-agent as a single high-level ``ask`` tool
(streamable-HTTP transport), in addition to the A2A and REST surfaces. It is the
"one capability, three transports" shape from the knowledge-agent-server plan.

Note: this exposes the *agent* (a high-level ask), not the raw sempkg retrieval
tools — those remain internal to the orchestration loop.
"""

from __future__ import annotations

import logging

from mcp.server.fastmcp import FastMCP

from .agent import KnowledgeAgent
from .config import Settings
from .render import render_clarification_markdown, render_result_markdown
from .schemas import ContextRequest

logger = logging.getLogger(__name__)


def build_mcp_app(agent: KnowledgeAgent, settings: Settings):
    """Return a streamable-HTTP ASGI app exposing the agent's ``ask`` tool."""
    mcp = FastMCP(
        name="sempkg-agent",
        host=settings.server.host,
        port=settings.server.port,
    )

    @mcp.tool(
        name="ask",
        description=(
            "Ask sempkg-agent for grounded, version-accurate context from installed "
            "packages. Returns the package, files, line ranges, verbatim snippets, "
            "selection reasoning, and a summary — or a clarifying question if the "
            "request is ambiguous. Pass `session_id` to continue a multi-turn "
            "clarification. Optionally scope to one `package`."
        ),
    )
    async def ask(prompt: str, package: str | None = None, session_id: str | None = None) -> str:
        request = ContextRequest(prompt=prompt, package=package, session_id=session_id)
        answer = await agent.ask(request)
        if answer.is_clarification():
            return render_clarification_markdown(answer.as_clarification())
        return render_result_markdown(answer.as_result())

    return mcp.streamable_http_app()

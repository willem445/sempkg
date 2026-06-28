"""Load the local ``sempkg`` MCP server's tools as LangChain tools.

The agent retrieves exclusively through the sempkg MCP server (CodeGraph + QMD /
LanceDB indexes), which guarantees version-accurate, grounded results.

Critically, we hold a SINGLE long-lived MCP session for the lifetime of the
process. The ``sempkg mcp`` server loads embedding / query-expansion / reranker
models on startup (a minutes-long, multi-GB operation), so a fresh subprocess per
tool call — the default behaviour of ``MultiServerMCPClient.get_tools()`` — would
make every retrieval unusably slow. Binding the tools to one warm session means
the models load exactly once, at startup.
"""

from __future__ import annotations

import contextlib
import logging
import os

from langchain_core.tools import BaseTool
from langchain_mcp_adapters.client import MultiServerMCPClient
from langchain_mcp_adapters.tools import load_mcp_tools

from .config import MCPSettings

logger = logging.getLogger(__name__)


def build_connections(settings: MCPSettings) -> dict[str, dict]:
    """Build the MultiServerMCPClient connection spec for the sempkg server.

    The subprocess inherits the parent environment so the sempkg reranker can
    read ``OPENROUTER_API_KEY`` (and any embedding/model env) exactly as it does
    when launched directly.
    """
    return {
        "sempkg": {
            "transport": "stdio",
            "command": settings.command,
            "args": settings.argv(),
            "env": dict(os.environ),
        }
    }


class SempkgToolProvider:
    """Owns one warm MCP session and exposes the curated sempkg tool surface."""

    def __init__(self, settings: MCPSettings) -> None:
        self._settings = settings
        self._client = MultiServerMCPClient(build_connections(settings))
        self._tools: list[BaseTool] | None = None
        self._stack = contextlib.AsyncExitStack()
        self._session = None

    async def load(self) -> list[BaseTool]:
        """Open the persistent session (once) and return the allowed tools."""
        if self._tools is not None:
            return self._tools

        logger.info(
            "Starting persistent sempkg MCP session (%s %s) — this loads models once "
            "and may take a while…",
            self._settings.command,
            " ".join(self._settings.argv()),
        )
        # One long-lived session: the sempkg subprocess stays warm for the whole
        # server lifetime instead of being relaunched per tool call.
        self._session = await self._stack.enter_async_context(self._client.session("sempkg"))
        all_tools = await load_mcp_tools(self._session)

        allowed = set(self._settings.allowed_tools)
        if allowed:
            tools = [t for t in all_tools if t.name in allowed]
            missing = allowed - {t.name for t in tools}
            if missing:
                logger.warning(
                    "Configured sempkg tools not exposed by server: %s",
                    ", ".join(sorted(missing)),
                )
        else:
            tools = list(all_tools)

        if not tools:
            raise RuntimeError(
                "sempkg MCP server exposed no usable tools. Verify `sempkg mcp -C "
                f"{self._settings.workspace}` runs and bundles are installed."
            )

        logger.info("Loaded %d sempkg MCP tools: %s", len(tools), ", ".join(t.name for t in tools))
        self._tools = tools
        return tools

    async def aclose(self) -> None:
        """Tear down the persistent session (terminates the sempkg subprocess)."""
        with contextlib.suppress(Exception):
            await self._stack.aclose()
        self._session = None
        self._tools = None

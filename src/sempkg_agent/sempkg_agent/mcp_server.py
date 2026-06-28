"""MCP-mount transport: expose retrieval over a network MCP server.

Two surfaces are served over streamable-HTTP, so a single hosted deployment
covers both audiences:

* ``ask`` — the high-level agent: a person-or-agent asks a question and gets a
  grounded, synthesised answer. Best for callers that want the agent to do the
  searching and reasoning.
* the **standard sempkg retrieval tools** (``query``, ``read_code``,
  ``read_docs``, ``read_symbol``, the call-graph tools, …), re-exported from the
  agent's warm sempkg session. This is the "just expose MCP" path: a capable
  calling agent drives the raw tools itself, over the network, against the same
  curated, version-pinned bundles — without the agent loop in the middle.

The raw ``sempkg mcp`` server is stdio-only; because the agent already holds one
warm session to it, we proxy the tools back out over HTTP here rather than
running a second Rust process.
"""

from __future__ import annotations

import logging

from mcp.server.fastmcp import FastMCP

from .agent import KnowledgeAgent
from .config import Settings
from .render import render_clarification_markdown, render_result_markdown
from .schemas import ContextRequest

logger = logging.getLogger(__name__)


def _register_passthrough(mcp: FastMCP, agent: KnowledgeAgent) -> None:
    """Re-export the raw sempkg retrieval tools that the agent has loaded.

    Wrappers are explicitly typed (clean MCP schemas) and registered only when the
    underlying tool is actually available in the agent's session, so the exposed
    surface matches the configured ``allowed_tools``.
    """
    available = {t.name for t in agent.raw_tools()}

    def expose(name: str):
        """Decorator: register ``fn`` as an MCP tool iff the sempkg tool exists."""

        def deco(fn):
            if name in available:
                mcp.add_tool(fn, name=name, description=(fn.__doc__ or "").strip())
            return fn

        return deco

    @expose("query")
    async def query(query: str, package: str | None = None, limit: int | None = None) -> str:
        """Unified reranked semantic+lexical search across installed bundles.
        Omit `package` to search everything; pass it to focus on one package."""
        args: dict = {"query": query}
        if package is not None:
            args["package"] = package
        if limit is not None:
            args["limit"] = limit
        return str(await agent.call_tool("query", args))

    @expose("list_packages")
    async def list_packages() -> str:
        """List installed packages/bundles and their versions."""
        return str(await agent.call_tool("list_packages", {}))

    @expose("list_files")
    async def list_files(package: str, filter: str | None = None, limit: int | None = None) -> str:
        """List files tracked in a package."""
        args: dict = {"package": package}
        if filter is not None:
            args["filter"] = filter
        if limit is not None:
            args["limit"] = limit
        return str(await agent.call_tool("list_files", args))

    @expose("read_code")
    async def read_code(package: str, file: str, line: int) -> str:
        """Read the source body of the symbol at `file`:`line` in a package."""
        args = {"package": package, "file": file, "line": line}
        return str(await agent.call_tool("read_code", args))

    @expose("read_symbol")
    async def read_symbol(package: str, symbol: str) -> str:
        """Read the full source body of a named symbol in a package."""
        return str(await agent.call_tool("read_symbol", {"package": package, "symbol": symbol}))

    @expose("read_docs")
    async def read_docs(
        package: str, file: str, start_line: int | None = None, end_line: int | None = None
    ) -> str:
        """Read a documentation file/section from a package."""
        args: dict = {"package": package, "file": file}
        if start_line is not None:
            args["start_line"] = start_line
        if end_line is not None:
            args["end_line"] = end_line
        return str(await agent.call_tool("read_docs", args))

    @expose("get_callers")
    async def get_callers(package: str, symbol: str, limit: int | None = None) -> str:
        """Find callers of a symbol (call-graph walk)."""
        args: dict = {"package": package, "symbol": symbol}
        if limit is not None:
            args["limit"] = limit
        return str(await agent.call_tool("get_callers", args))

    @expose("get_callees")
    async def get_callees(package: str, symbol: str, limit: int | None = None) -> str:
        """Find callees of a symbol (call-graph walk)."""
        args: dict = {"package": package, "symbol": symbol}
        if limit is not None:
            args["limit"] = limit
        return str(await agent.call_tool("get_callees", args))

    @expose("get_impact")
    async def get_impact(package: str, symbol: str, depth: int | None = None) -> str:
        """Analyse downstream impact of changing a symbol."""
        args: dict = {"package": package, "symbol": symbol}
        if depth is not None:
            args["depth"] = depth
        return str(await agent.call_tool("get_impact", args))

    exposed = sorted(available & {
        "query", "list_packages", "list_files", "read_code", "read_symbol",
        "read_docs", "get_callers", "get_callees", "get_impact",
    })
    logger.info("MCP mount re-exporting raw sempkg tools: %s", ", ".join(exposed) or "(none)")


def build_mcp_app(agent: KnowledgeAgent, settings: Settings):
    """Return a streamable-HTTP ASGI app exposing ``ask`` + the raw sempkg tools."""
    mcp = FastMCP(
        name="sempkg-agent",
        host=settings.server.host,
        port=settings.server.port,
    )

    @mcp.tool(
        name="ask",
        description=(
            "Ask sempkg-agent for a grounded, version-accurate answer from installed "
            "packages. Returns a human-readable answer plus cited sources (package, "
            "files, line ranges, verbatim snippets) — or a clarifying question if the "
            "request is ambiguous. Pass `session_id` to continue a multi-turn "
            "conversation. Optionally scope to one `package` and/or a `version`."
        ),
    )
    async def ask(
        prompt: str,
        package: str | None = None,
        version: str | None = None,
        session_id: str | None = None,
    ) -> str:
        request = ContextRequest(
            prompt=prompt, package=package, version=version, session_id=session_id
        )
        answer = await agent.ask(request)
        if answer.is_clarification():
            return render_clarification_markdown(answer.as_clarification())
        return render_result_markdown(answer.as_result())

    # Re-export the standard sempkg retrieval tools for agents that prefer to drive
    # retrieval themselves (the "expose MCP directly" path).
    _register_passthrough(mcp, agent)

    return mcp.streamable_http_app()

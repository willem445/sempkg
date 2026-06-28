"""Command-line entrypoint for the sempkg agent server.

Examples
--------
Run the primary A2A transport::

    sempkg-agent serve --transport a2a

Run a simple REST endpoint for local curl testing::

    sempkg-agent serve --transport rest

Run all three transports at once for local testing (A2A on PORT, REST on PORT+1,
MCP on PORT+2)::

    sempkg-agent serve --transport all

In production, prefer one transport per process/container for independent scaling.
"""

from __future__ import annotations

import argparse
import asyncio
import logging

from .agent import KnowledgeAgent
from .config import Settings, get_settings
from .logging_config import configure_logging

logger = logging.getLogger(__name__)


def _build_app(transport: str, agent: KnowledgeAgent, settings: Settings):
    if transport == "a2a":
        from .a2a_server import build_a2a_app

        return build_a2a_app(agent, settings)
    if transport == "rest":
        from .rest import build_rest_app

        return build_rest_app(agent, settings)
    if transport == "mcp":
        from .mcp_server import build_mcp_app

        return build_mcp_app(agent, settings)
    raise ValueError(f"Unknown transport: {transport}")


async def _serve_one(app, host: str, port: int, log_level: str) -> None:
    import uvicorn

    config = uvicorn.Config(
        app,
        host=host,
        port=port,
        log_level=log_level.lower(),
        lifespan="auto",  # required for the MCP streamable-HTTP session manager
    )
    await uvicorn.Server(config).serve()


async def _run(settings: Settings, transport: str) -> None:
    logger.info("Initialising KnowledgeAgent (model=%s)…", settings.llm.model)
    agent = await KnowledgeAgent.create(settings)

    host = settings.server.host
    base_port = settings.server.port
    level = settings.server.log_level

    try:
        if transport == "all":
            # A2A (primary) on base_port, REST on +1, MCP on +2 — shared agent.
            from .a2a_server import build_a2a_app
            from .mcp_server import build_mcp_app
            from .rest import build_rest_app

            servers = [
                _serve_one(build_a2a_app(agent, settings), host, base_port, level),
                _serve_one(build_rest_app(agent, settings), host, base_port + 1, level),
                _serve_one(build_mcp_app(agent, settings), host, base_port + 2, level),
            ]
            logger.info(
                "Serving A2A=%d  REST=%d  MCP=%d on %s",
                base_port,
                base_port + 1,
                base_port + 2,
                host,
            )
            await asyncio.gather(*servers)
        else:
            app = _build_app(transport, agent, settings)
            logger.info("Serving %s transport on %s:%d", transport.upper(), host, base_port)
            await _serve_one(app, host, base_port, level)
    finally:
        # Best-effort teardown of the warm sempkg subprocess (same task as setup).
        await agent.aclose()


def _cmd_serve(args: argparse.Namespace) -> None:
    settings = get_settings()
    configure_logging(settings.server.log_level)
    try:
        asyncio.run(_run(settings, args.transport))
    except KeyboardInterrupt:
        logger.info("Shutting down.")


def _cmd_card(args: argparse.Namespace) -> None:
    """Print the A2A AgentCard JSON (useful for registration/debugging)."""
    import json

    from .a2a_server import build_agent_card

    settings = get_settings()
    card = build_agent_card(settings)
    print(json.dumps(card.model_dump(by_alias=True, exclude_none=True), indent=2))


def main() -> None:
    parser = argparse.ArgumentParser(
        prog="sempkg-agent",
        description="Grounded code-intelligence agent server over sempkg bundles.",
    )
    sub = parser.add_subparsers(dest="command", required=True)

    serve = sub.add_parser("serve", help="Start the agent server")
    serve.add_argument(
        "--transport",
        choices=["a2a", "rest", "mcp", "all"],
        default="a2a",
        help="Inbound protocol to serve (default: a2a). 'all' runs every transport.",
    )
    serve.set_defaults(func=_cmd_serve)

    card = sub.add_parser("card", help="Print the A2A AgentCard JSON and exit")
    card.set_defaults(func=_cmd_card)

    args = parser.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()

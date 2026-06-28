"""A2A application assembly: AgentCard + request handler + Starlette app.

Build the public A2A surface advertising a single ``retrieve_package_context``
skill. Other agents discover the card at ``/.well-known/agent.json`` and open
tasks (one-shot or multi-turn with clarification) against it.
"""

from __future__ import annotations

from a2a.server.apps import A2AStarletteApplication
from a2a.server.request_handlers import DefaultRequestHandler
from a2a.server.tasks import InMemoryTaskStore
from a2a.types import AgentCapabilities, AgentCard, AgentSkill

from .a2a_executor import KnowledgeAgentExecutor
from .agent import KnowledgeAgent
from .config import Settings


def build_agent_card(settings: Settings) -> AgentCard:
    skill = AgentSkill(
        id="retrieve_package_context",
        name="Retrieve package context",
        description=(
            "Given a natural-language request, perform version-accurate semantic "
            "search across installed sembundles and return exactly the needed "
            "context: package, files, line ranges, verbatim snippets, the reasoning "
            "behind the selection, and a summary. Asks a clarifying question when the "
            "request is ambiguous."
        ),
        tags=["code-intelligence", "rag", "retrieval", "sempkg", "grounded"],
        examples=[
            "How does query expansion route lexical vs vector variants in sempkg?",
            "Show me how pandas implements DataFrame.merge join keys.",
            "Where does lancedb open the BM25 index? Scope to the lancedb package.",
        ],
    )
    return AgentCard(
        name="sempkg-agent",
        description=(
            "Grounded code-intelligence agent. Returns version-pinned context from "
            "installed sembundles for calling agents."
        ),
        url=settings.server.public_url,
        version="0.1.0",
        defaultInputModes=["text", "data"],
        defaultOutputModes=["text", "data"],
        capabilities=AgentCapabilities(streaming=True, pushNotifications=False),
        skills=[skill],
    )


def build_a2a_app(agent: KnowledgeAgent, settings: Settings):
    """Return a Starlette ASGI app exposing the agent over A2A."""
    executor = KnowledgeAgentExecutor(agent)
    request_handler = DefaultRequestHandler(
        agent_executor=executor,
        task_store=InMemoryTaskStore(),
    )
    app = A2AStarletteApplication(
        agent_card=build_agent_card(settings),
        http_handler=request_handler,
    )
    return app.build()

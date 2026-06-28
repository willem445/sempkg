"""Functional test: the agent server answering over a real sempkg backend.

Boots the actual ``KnowledgeAgent`` (which launches ``sempkg mcp`` and loads the
real CodeGraph/LanceDB indexes) and drives the REST server in-process through an
ASGI client — the same path a browser/curl hits — proving the full loop:

    HTTP /v1/ask  →  LangGraph ReAct  →  sempkg MCP retrieval  →  grounded answer

This makes **paid** OpenRouter calls and is slow (the sempkg backend loads models
on startup), so it is opt-in. It runs only when ALL of these hold:
- ``SEMPKG_AGENT_FUNCTIONAL=1`` is set (explicit acknowledgement of cost);
- an OpenRouter key is configured (``OPENROUTER_API_KEY`` / ``SEMPKG_AGENT_API_KEY``);
- the ``sempkg`` binary is built;
- a workspace with installed bundles is available
  (``SEMPKG_AGENT_MCP_WORKSPACE`` / ``SEMPKG_WORKSPACE``; defaults to the repo root).

Run with:  SEMPKG_AGENT_FUNCTIONAL=1 OPENROUTER_API_KEY=sk-or-... pytest -m functional -k backend
"""

from __future__ import annotations

import os

import pytest
from httpx import ASGITransport, AsyncClient

from sempkg_agent.agent import KnowledgeAgent
from sempkg_agent.config import Settings
from sempkg_agent.rest import build_rest_app

pytestmark = pytest.mark.functional

# A model + prompt aimed at the sempkg codebase corpus; override for other workspaces.
_DEFAULT_MODEL = os.environ.get("SEMPKG_AGENT_FUNCTIONAL_MODEL", "openai/gpt-4o-mini")
_DEFAULT_PROMPT = os.environ.get(
    "SEMPKG_AGENT_FUNCTIONAL_PROMPT",
    "How does the query tool combine and rerank results before returning them?",
)


def _require_opt_in() -> None:
    if os.environ.get("SEMPKG_AGENT_FUNCTIONAL") != "1":
        pytest.skip("set SEMPKG_AGENT_FUNCTIONAL=1 to run the paid agent backend test")
    if not (os.environ.get("OPENROUTER_API_KEY") or os.environ.get("SEMPKG_AGENT_API_KEY")):
        pytest.skip("OPENROUTER_API_KEY (or SEMPKG_AGENT_API_KEY) required for this test")


def _workspace() -> str:
    return (
        os.environ.get("SEMPKG_AGENT_MCP_WORKSPACE")
        or os.environ.get("SEMPKG_WORKSPACE")
        or str(__import__("pathlib").Path(__file__).resolve().parents[3])
    )


async def test_agent_server_answers_grounded_over_real_backend(sempkg_bin: str) -> None:
    _require_opt_in()

    settings = Settings()
    settings.mcp.command = sempkg_bin          # use the built binary, not PATH
    settings.mcp.workspace = _workspace()
    settings.llm.model = _DEFAULT_MODEL         # cheap model to bound cost
    settings.agent.mode = "human"

    agent = await KnowledgeAgent.create(settings)  # launches sempkg mcp + loads models
    try:
        app = build_rest_app(agent, settings)
        transport = ASGITransport(app=app)
        async with AsyncClient(transport=transport, base_url="http://agent.test") as client:
            # The server is up and reports its persona + installed corpus.
            assert (await client.get("/healthz")).json()["status"] == "ok"
            cfg = (await client.get("/v1/config")).json()
            assert cfg["mode"] == "human"
            assert cfg["installed"], "agent reports no installed packages — empty workspace?"

            # Ask a question; expect a grounded, cited answer from the real backend.
            resp = await client.post("/v1/ask", json={"prompt": _DEFAULT_PROMPT})
            assert resp.status_code == 200, resp.text
            body = resp.json()
            assert body["kind"] == "context_result", body
            result = body["result"]

            assert result["packages_searched"], "agent did not record any searched packages"
            assert result["findings"], "expected grounded findings from the sempkg backend"

            # Every cited snippet went through the deterministic verification pass.
            assert all(f["verified"] is not None for f in result["findings"]), (
                "findings should carry a verified flag (verification enabled by default)"
            )
            # At least one citation must verify against the retrieved evidence.
            assert any(f["verified"] for f in result["findings"]), (
                "no finding could be verified against retrieved evidence — "
                "possible hallucinated citations"
            )

            # The human answer is prose with the package provenance on each source.
            assert result["answer"], "human mode should return a prose answer"
            assert all(f["package"] for f in result["findings"])
    finally:
        await agent.aclose()

"""REST transport integration tests with a mocked agent (no LLM, no network)."""

from __future__ import annotations

from fastapi.testclient import TestClient
from pydantic import SecretStr

from sempkg_agent.config import Settings
from sempkg_agent.rest import build_rest_app
from sempkg_agent.schemas import AgentAnswer, ContextRequest, Finding


class _StubAgent:
    """Returns a preset AgentAnswer and records the request it received."""

    def __init__(self, answer: AgentAnswer) -> None:
        self._answer = answer
        self.last_request: ContextRequest | None = None

    async def ask(self, request: ContextRequest) -> AgentAnswer:
        self.last_request = request
        return self._answer


def _client(answer: AgentAnswer, settings: Settings | None = None) -> tuple[TestClient, _StubAgent]:
    settings = settings or Settings()
    agent = _StubAgent(answer)
    app = build_rest_app(agent, settings)
    return TestClient(app), agent


def test_healthz() -> None:
    client, _ = _client(AgentAnswer(kind="context_result", summary="s", reasoning="r"))
    resp = client.get("/healthz")
    assert resp.status_code == 200
    assert resp.json()["status"] == "ok"


def test_ask_returns_context_result() -> None:
    answer = AgentAnswer(
        kind="context_result",
        summary="found merge",
        reasoning="queried pandas",
        packages_searched=["pandas"],
        findings=[
            Finding(
                package="pandas",
                file="core/reshape/merge.py",
                start_line=120,
                end_line=180,
                snippet="def merge(): ...",
                explanation="entry point",
            )
        ],
    )
    client, agent = _client(answer)
    resp = client.post("/v1/ask", json={"prompt": "how does merge work?", "package": "pandas"})
    assert resp.status_code == 200
    body = resp.json()
    assert body["kind"] == "context_result"
    assert body["result"]["findings"][0]["package"] == "pandas"
    assert "def merge" in body["markdown"]
    # The scope hint and a generated session id propagate into the request.
    assert agent.last_request.package == "pandas"
    assert body["session_id"].startswith("rest-")


def test_ask_clarification_flow() -> None:
    answer = AgentAnswer(
        kind="clarification",
        clarifying_question="Which pandas version?",
        clarification_rationale="two installed",
    )
    client, _ = _client(answer)
    resp = client.post("/v1/ask", json={"prompt": "merge", "session_id": "abc"})
    assert resp.status_code == 200
    body = resp.json()
    assert body["kind"] == "clarification"
    assert body["clarification"]["question"] == "Which pandas version?"
    assert body["session_id"] == "abc"  # caller-provided session id preserved


def test_auth_token_enforced() -> None:
    settings = Settings()
    settings.server.auth_token = SecretStr("secret")
    client, _ = _client(
        AgentAnswer(kind="context_result", summary="s", reasoning="r"), settings
    )
    # Missing token -> 401
    assert client.post("/v1/ask", json={"prompt": "x"}).status_code == 401
    # Correct token -> 200
    ok = client.post(
        "/v1/ask", json={"prompt": "x"}, headers={"Authorization": "Bearer secret"}
    )
    assert ok.status_code == 200

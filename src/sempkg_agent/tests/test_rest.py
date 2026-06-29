"""REST transport integration tests with a mocked agent (no LLM, no network)."""

from __future__ import annotations

import json

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

    async def list_installed(self) -> str:
        return "ourcode  v14.2.0\nourcode  v14.1.0"

    async def astream(self, request: ContextRequest):
        self.last_request = request
        sid = request.session_id or "rest-x"
        yield {"type": "status", "state": "working", "model": request.model or "default",
               "session_id": sid}
        yield {"type": "tool_call", "name": "query", "input": "{}"}
        yield {"type": "tool_result", "name": "query", "output": "results"}
        if self._answer.is_clarification():
            clar = self._answer.as_clarification()
            yield {"type": "clarification", "question": clar.question,
                   "rationale": clar.rationale, "session_id": sid}
        else:
            res = self._answer.as_result()
            yield {"type": "final", "result": res.model_dump(), "markdown": "md", "session_id": sid}
        yield {"type": "done"}


def _parse_sse(text: str) -> list[dict]:
    events = []
    for frame in text.split("\n\n"):
        for line in frame.splitlines():
            if line.startswith("data:"):
                events.append(json.loads(line[5:].strip()))
    return events


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


def test_models_endpoint_lists_catalog() -> None:
    client, _ = _client(AgentAnswer(kind="context_result", summary="s", reasoning="r"))
    resp = client.get("/v1/models")
    assert resp.status_code == 200
    body = resp.json()
    assert "default" in body and isinstance(body["models"], list) and body["models"]
    tiers = {m["tier"] for m in body["models"]}
    assert {"cheap", "high"} & tiers  # curated tiers present
    # The configured default model is always selectable.
    assert any(m["id"] == body["default"] for m in body["models"])


def test_chat_ui_is_served() -> None:
    client, _ = _client(AgentAnswer(kind="context_result", summary="s", reasoning="r"))
    resp = client.get("/")
    assert resp.status_code == 200
    assert "text/html" in resp.headers["content-type"]
    assert "knowledge agent" in resp.text.lower()


def test_config_endpoint_exposes_branding_and_installed() -> None:
    settings = Settings()
    settings.agent.ui_title = "Acme Knowledge"
    client, _ = _client(AgentAnswer(kind="context_result", summary="s", reasoning="r"), settings)
    data = client.get("/v1/config").json()
    assert data["mode"] == "human"
    assert data["title"] == "Acme Knowledge"
    assert "v14.2.0" in data["installed"]
    assert data["verify_citations"] is True


def test_version_scope_is_forwarded() -> None:
    client, agent = _client(AgentAnswer(kind="context_result", summary="s", reasoning="r"))
    client.post("/v1/ask", json={"prompt": "how does X work?", "version": "v14.2.0"})
    assert agent.last_request.version == "v14.2.0"


def test_ask_stream_emits_events() -> None:
    answer = AgentAnswer(
        kind="context_result",
        summary="streamed answer",
        reasoning="r",
        findings=[
            Finding(package="pandas", file="m.py", start_line=1, end_line=2,
                    snippet="def merge(): ...", explanation="x")
        ],
    )
    client, agent = _client(answer)
    resp = client.post("/v1/ask/stream", json={"prompt": "merge?", "model": "openai/gpt-4o-mini"})
    assert resp.status_code == 200
    assert "text/event-stream" in resp.headers["content-type"]
    events = _parse_sse(resp.text)
    types = [e["type"] for e in events]
    assert types[0] == "status" and types[-1] == "done"
    assert "tool_call" in types and "tool_result" in types
    final = next(e for e in events if e["type"] == "final")
    assert final["result"]["summary"] == "streamed answer"
    # Per-request model selection propagates into the request.
    assert agent.last_request.model == "openai/gpt-4o-mini"


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

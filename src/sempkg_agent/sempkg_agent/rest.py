"""REST transport: a thin, curl-able HTTP/JSON surface for local testing.

This is intentionally simple — one ``POST /v1/ask`` endpoint plus health. It
returns BOTH the structured contract and a markdown rendering. Multi-turn
clarification works by passing the same ``session_id`` back in the next request.
"""

from __future__ import annotations

import logging
import secrets
import uuid

from fastapi import Depends, FastAPI, Header, HTTPException
from pydantic import BaseModel, Field

from .agent import KnowledgeAgent
from .config import Settings
from .render import render_clarification_markdown, render_result_markdown
from .schemas import ClarificationRequest, ContextRequest, ContextResult

logger = logging.getLogger(__name__)


class AskRequest(BaseModel):
    prompt: str = Field(..., min_length=1)
    package: str | None = None
    session_id: str | None = None
    max_findings: int | None = Field(default=None, ge=1, le=50)


class AskResponse(BaseModel):
    session_id: str
    kind: str
    result: ContextResult | None = None
    clarification: ClarificationRequest | None = None
    markdown: str


def build_rest_app(agent: KnowledgeAgent, settings: Settings) -> FastAPI:
    app = FastAPI(
        title="sempkg-agent",
        version="0.1.0",
        description="Grounded code-intelligence agent over sempkg bundles (REST transport).",
    )

    def require_auth(authorization: str | None = Header(default=None)) -> None:
        expected = settings.server.auth_token_value
        if not expected:
            return  # auth disabled
        if not authorization or not authorization.startswith("Bearer "):
            raise HTTPException(status_code=401, detail="Missing or malformed Authorization header")
        provided = authorization.removeprefix("Bearer ").strip()
        if not secrets.compare_digest(provided, expected):
            raise HTTPException(status_code=401, detail="Invalid token")

    @app.get("/healthz")
    def healthz() -> dict:
        return {"status": "ok", "model": settings.llm.model}

    @app.post("/v1/ask", response_model=AskResponse, dependencies=[Depends(require_auth)])
    async def ask(body: AskRequest) -> AskResponse:
        session_id = body.session_id or f"rest-{uuid.uuid4().hex}"
        request = ContextRequest(
            prompt=body.prompt,
            package=body.package,
            session_id=session_id,
            max_findings=body.max_findings,
        )
        try:
            answer = await agent.ask(request)
        except Exception as exc:  # noqa: BLE001 - return a clean 502 to the caller
            logger.exception("Agent run failed")
            raise HTTPException(status_code=502, detail=f"Retrieval failed: {exc}") from exc

        if answer.is_clarification():
            clar = answer.as_clarification()
            return AskResponse(
                session_id=session_id,
                kind="clarification",
                clarification=clar,
                markdown=render_clarification_markdown(clar),
            )
        result = answer.as_result()
        return AskResponse(
            session_id=session_id,
            kind="context_result",
            result=result,
            markdown=render_result_markdown(result),
        )

    return app

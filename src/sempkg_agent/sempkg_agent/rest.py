"""REST transport + optional chat UI.

Endpoints:
* ``GET  /``               — the chat UI (single static page).
* ``GET  /healthz``        — liveness + default model.
* ``GET  /v1/models``      — the selectable model catalog (for the UI dropdown).
* ``POST /v1/ask``         — one-shot: returns the structured answer + markdown.
* ``POST /v1/ask/stream``  — SSE: live reasoning + tool calls, then the final answer.

Multi-turn clarification works by passing the same ``session_id`` back.
"""

from __future__ import annotations

import json
import logging
import secrets
import uuid
from pathlib import Path

from fastapi import Depends, FastAPI, Header, HTTPException
from fastapi.responses import FileResponse, StreamingResponse
from pydantic import BaseModel, Field

from .agent import KnowledgeAgent
from .config import Settings
from .models import catalog_as_dicts
from .render import render_clarification_markdown, render_result_markdown
from .schemas import ClarificationRequest, ContextRequest, ContextResult

logger = logging.getLogger(__name__)

_UI_PATH = Path(__file__).resolve().parent / "static" / "index.html"


class AskRequest(BaseModel):
    prompt: str = Field(..., min_length=1)
    package: str | None = None
    session_id: str | None = None
    max_findings: int | None = Field(default=None, ge=1, le=50)
    model: str | None = None


class AskResponse(BaseModel):
    session_id: str
    kind: str
    result: ContextResult | None = None
    clarification: ClarificationRequest | None = None
    markdown: str


def _sse(event: dict) -> str:
    """Format one event as an SSE ``data:`` frame."""
    return f"data: {json.dumps(event)}\n\n"


def build_rest_app(agent: KnowledgeAgent, settings: Settings) -> FastAPI:
    app = FastAPI(
        title="sempkg-agent",
        version="0.1.0",
        description="Grounded code-intelligence agent over sempkg bundles (REST + chat UI).",
    )
    default_model = settings.llm.model

    def require_auth(authorization: str | None = Header(default=None)) -> None:
        expected = settings.server.auth_token_value
        if not expected:
            return  # auth disabled
        if not authorization or not authorization.startswith("Bearer "):
            raise HTTPException(status_code=401, detail="Missing or malformed Authorization header")
        provided = authorization.removeprefix("Bearer ").strip()
        if not secrets.compare_digest(provided, expected):
            raise HTTPException(status_code=401, detail="Invalid token")

    @app.get("/")
    def chat_ui() -> FileResponse:
        if not _UI_PATH.exists():  # pragma: no cover - packaging guard
            raise HTTPException(status_code=404, detail="Chat UI asset not found")
        return FileResponse(str(_UI_PATH), media_type="text/html")

    @app.get("/healthz")
    def healthz() -> dict:
        return {"status": "ok", "model": default_model}

    @app.get("/v1/models")
    def list_models() -> dict:
        return {"default": default_model, "models": catalog_as_dicts(default_model)}

    @app.post("/v1/ask", response_model=AskResponse, dependencies=[Depends(require_auth)])
    async def ask(body: AskRequest) -> AskResponse:
        session_id = body.session_id or f"rest-{uuid.uuid4().hex}"
        request = ContextRequest(
            prompt=body.prompt,
            package=body.package,
            session_id=session_id,
            max_findings=body.max_findings,
            model=body.model,
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

    @app.post("/v1/ask/stream", dependencies=[Depends(require_auth)])
    async def ask_stream(body: AskRequest) -> StreamingResponse:
        session_id = body.session_id or f"rest-{uuid.uuid4().hex}"
        request = ContextRequest(
            prompt=body.prompt,
            package=body.package,
            session_id=session_id,
            max_findings=body.max_findings,
            model=body.model,
        )

        async def event_source():
            try:
                async for event in agent.astream(request):
                    yield _sse(event)
            except Exception as exc:  # noqa: BLE001 - emit a terminal error frame
                logger.exception("Streaming run failed")
                yield _sse({"type": "error", "message": str(exc)})
                yield _sse({"type": "done"})

        return StreamingResponse(
            event_source(),
            media_type="text/event-stream",
            headers={
                "Cache-Control": "no-cache",
                "Connection": "keep-alive",
                "X-Accel-Buffering": "no",  # disable proxy buffering (nginx)
            },
        )

    return app

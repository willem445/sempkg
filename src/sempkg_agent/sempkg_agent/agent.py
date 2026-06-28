"""The knowledge agent: a LangGraph ReAct loop over the sempkg MCP tools.

The agent is provider-agnostic (any OpenAI-compatible / OpenRouter model) and
emits a single structured ``AgentAnswer`` per turn. Multi-turn clarification is
supported via a LangGraph checkpointer keyed by ``session_id``: when the caller
replies to a clarifying question with the same session id, the full conversation
history is replayed so the agent continues where it left off.
"""

from __future__ import annotations

import asyncio
import json
import logging
import uuid
from collections.abc import AsyncIterator

from langchain_core.messages import AIMessage, HumanMessage, SystemMessage, ToolMessage
from langchain_openai import ChatOpenAI
from langgraph.checkpoint.memory import MemorySaver
from langgraph.errors import GraphRecursionError
from langgraph.prebuilt import create_react_agent

from .config import Settings
from .mcp_tools import SempkgToolProvider
from .models import is_allowed
from .prompts import SYSTEM_PROMPT, build_user_message
from .render import render_result_markdown
from .schemas import AgentAnswer, ContextRequest
from .streaming import StreamCallback
from .tracing import AgentTracer

logger = logging.getLogger(__name__)

# Sentinel marking the end of the streaming queue.
_STREAM_DONE = object()


def _collect_tool_evidence(messages: list) -> list[tuple[str, dict, str]]:
    """Extract (tool_name, args, result_text) triples from a ReAct history.

    Pairs each ToolMessage with the tool call that produced it so the synthesis
    step sees clean, attributed evidence instead of the raw conversation.
    """
    pending: dict = {}
    evidence: list[tuple[str, dict, str]] = []
    for m in messages:
        if isinstance(m, AIMessage):
            for tc in m.tool_calls or []:
                pending[tc.get("id")] = (tc.get("name", "tool"), tc.get("args", {}))
        elif isinstance(m, ToolMessage):
            name, args = pending.get(m.tool_call_id, ("tool", {}))
            evidence.append((name, args, str(m.content)))
    return evidence


def _coerce_to_result(answer: AgentAnswer) -> AgentAnswer:
    """Force a synthesis answer into context_result shape.

    Weaker models sometimes return kind='clarification' while populating result
    fields (or vice versa). The fallback must answer, never clarify, so we rebuild
    it as a context_result, preserving any findings/summary the model provided.
    """
    if answer.kind == "context_result":
        return answer
    summary = answer.summary or answer.clarifying_question or (
        "Returned the context gathered before the search was stopped."
    )
    return AgentAnswer(
        kind="context_result",
        summary=summary,
        reasoning=answer.reasoning or "Synthesized from context gathered before stopping.",
        packages_searched=answer.packages_searched,
        findings=answer.findings,
    )


class KnowledgeAgent:
    """Orchestrates grounded retrieval and structured answer assembly."""

    def __init__(self, settings: Settings, tool_provider: SempkgToolProvider) -> None:
        self._settings = settings
        self._tool_provider = tool_provider
        self._tools = None  # loaded in setup()
        self._checkpointer = None  # shared across per-model graphs
        # One ReAct graph per model id (tools + checkpointer shared).
        self._graphs: dict[str, object] = {}
        self._models: dict[str, ChatOpenAI] = {}
        self._default_model_name = settings.llm.model

    @classmethod
    async def create(cls, settings: Settings) -> KnowledgeAgent:
        """Construct and fully initialise an agent (loads MCP tools + builds graph)."""
        agent = cls(settings, SempkgToolProvider(settings.mcp))
        await agent.setup()
        return agent

    async def setup(self) -> None:
        """Load tools and warm the default model's graph. Idempotent."""
        if self._tools is not None:
            return
        self._tools = await self._tool_provider.load()
        # MemorySaver keeps per-session conversation state in-process, shared
        # across models so switching model mid-conversation keeps history. For a
        # horizontally-scaled deployment, swap in a persistent checkpointer
        # (e.g. langgraph's Postgres/Redis saver) — the interface is identical.
        self._checkpointer = MemorySaver()
        self._ensure_api_key()
        self._graph_for(self._default_model_name)  # warm the default
        logger.info("KnowledgeAgent ready (default model=%s)", self._default_model_name)

    def _ensure_api_key(self) -> None:
        if not self._settings.llm.api_key_value:
            # Fail fast with a clear message rather than a cryptic 401 mid-request.
            raise RuntimeError(
                "No LLM API key configured. Set OPENROUTER_API_KEY (or "
                "SEMPKG_AGENT_API_KEY) before starting the agent server."
            )

    def _build_model(self, model_name: str) -> ChatOpenAI:
        llm = self._settings.llm
        self._ensure_api_key()
        return ChatOpenAI(
            model=model_name,
            base_url=llm.api_base,
            api_key=llm.api_key_value,
            temperature=llm.temperature,
            max_tokens=llm.max_tokens,
            timeout=llm.request_timeout,
            max_retries=llm.max_retries,
        )

    def _graph_for(self, model_name: str):
        """Get-or-build the ReAct graph for a model (tools + checkpointer shared)."""
        if model_name not in self._graphs:
            model = self._build_model(model_name)
            self._models[model_name] = model
            self._graphs[model_name] = create_react_agent(
                model,
                self._tools,
                prompt=SYSTEM_PROMPT,
                response_format=AgentAnswer,
                checkpointer=self._checkpointer,
            )
            logger.info("Built agent graph for model=%s", model_name)
        return self._graphs[model_name], self._models[model_name]

    def _resolve_model_name(self, request: ContextRequest) -> str:
        """Pick the model for this request, validated against the catalog."""
        requested = request.model
        if requested and is_allowed(requested, self._default_model_name):
            return requested
        if requested:
            logger.warning(
                "Requested model %r not in catalog; using default %s",
                requested,
                self._default_model_name,
            )
        return self._default_model_name

    def _make_config(self, thread_id: str, extra_callbacks: list | None = None) -> dict:
        config: dict = {
            "configurable": {"thread_id": thread_id},
            "recursion_limit": self._settings.agent.max_iterations * 2 + 5,
        }
        callbacks = list(extra_callbacks or [])
        if self._settings.agent.trace:
            callbacks.append(AgentTracer())
        if callbacks:
            config["callbacks"] = callbacks
        return config

    async def ask(self, request: ContextRequest) -> AgentAnswer:
        """Run one turn and return the structured answer.

        ``request.session_id`` ties multi-turn clarification together; when
        omitted a fresh, single-shot session id is generated.
        """
        if self._tools is None:
            raise RuntimeError("KnowledgeAgent.setup() must be called before ask().")

        model_name = self._resolve_model_name(request)
        graph, model = self._graph_for(model_name)
        thread_id = request.session_id or f"oneshot-{uuid.uuid4().hex}"
        user_text = build_user_message(request.prompt, request.package)

        logger.info(
            "ask: thread=%s model=%s package=%s prompt=%r",
            thread_id,
            model_name,
            request.package,
            request.prompt[:200],
        )

        config = self._make_config(thread_id)
        answer = await self._invoke(graph, model, config, user_text)
        return self._postprocess(answer, request)

    async def astream(self, request: ContextRequest) -> AsyncIterator[dict]:
        """Run one turn, yielding live events (reasoning / tool calls / result).

        Event ``type`` is one of: ``status``, ``reasoning``, ``tool_call``,
        ``tool_result``, ``tool_error``, ``final``, ``clarification``, ``error``,
        ``done``. The final structured answer arrives as ``final`` (or
        ``clarification``); ``done`` always terminates the stream.
        """
        if self._tools is None:
            raise RuntimeError("KnowledgeAgent.setup() must be called before astream().")

        model_name = self._resolve_model_name(request)
        graph, model = self._graph_for(model_name)
        thread_id = request.session_id or f"oneshot-{uuid.uuid4().hex}"
        user_text = build_user_message(request.prompt, request.package)
        logger.info(
            "astream: thread=%s model=%s prompt=%r", thread_id, model_name, request.prompt[:200]
        )

        queue: asyncio.Queue = asyncio.Queue()
        config = self._make_config(thread_id, extra_callbacks=[StreamCallback(queue)])

        yield {"type": "status", "state": "working", "model": model_name, "session_id": thread_id}

        outcome: dict = {}

        async def _run() -> None:
            try:
                answer = await self._invoke(graph, model, config, user_text)
                outcome["answer"] = self._postprocess(answer, request)
            except Exception as exc:  # noqa: BLE001 - surfaced as an error event
                logger.exception("Streaming run failed")
                outcome["error"] = str(exc)
            finally:
                await queue.put(_STREAM_DONE)

        task = asyncio.create_task(_run())
        try:
            while True:
                item = await queue.get()
                if item is _STREAM_DONE:
                    break
                yield item
        finally:
            await task

        if "error" in outcome:
            yield {"type": "error", "message": outcome["error"]}
        else:
            answer = outcome["answer"]
            if answer.is_clarification():
                clar = answer.as_clarification()
                yield {
                    "type": "clarification",
                    "question": clar.question,
                    "rationale": clar.rationale,
                    "session_id": thread_id,
                }
            else:
                result = answer.as_result()
                yield {
                    "type": "final",
                    "result": result.model_dump(),
                    "markdown": render_result_markdown(result),
                    "session_id": thread_id,
                }
        yield {"type": "done"}

    async def _invoke(self, graph, model, config: dict, user_text: str) -> AgentAnswer:
        """Run the graph once, with the recursion-limit synthesis fallback."""
        thread_id = config["configurable"]["thread_id"]
        try:
            state = await graph.ainvoke(
                {"messages": [HumanMessage(content=user_text)]},
                config=config,
            )
            answer = state.get("structured_response")
        except GraphRecursionError:
            # The model kept calling tools without converging (common with
            # smaller/faster models and the expensive `query` tool). Salvage a
            # grounded answer by forcing one structured synthesis over everything
            # retrieved so far, rather than failing the request.
            logger.warning("Recursion limit hit for thread=%s; forcing synthesis.", thread_id)
            answer = await self._synthesize_from_state(graph, model, config, user_text)

        if not isinstance(answer, AgentAnswer):
            raise RuntimeError(
                "Agent did not produce a structured answer; the model may not support "
                "structured output. Try a different model."
            )
        return answer

    async def _synthesize_from_state(
        self, graph, model, config: dict, request_text: str
    ) -> AgentAnswer:
        """Force a final grounded answer from the context gathered so far.

        Used when the ReAct loop fails to terminate. Rather than replaying the raw
        tool-call conversation (which confuses weaker models and risks provider
        tool-pairing errors), we extract the gathered tool RESULTS into a clean
        evidence block and ask for a one-shot ``context_result``. The fallback
        never asks a clarification — it answers with whatever was found.
        """
        snapshot = await graph.aget_state(config)
        raw = list(snapshot.values.get("messages", []))
        evidence = _collect_tool_evidence(raw)
        logger.info(
            "Synthesis fallback: %d history messages, %d tool result(s) as evidence.",
            len(raw),
            len(evidence),
        )

        if evidence:
            ev_text = "\n\n".join(
                f"### Result from `{name}({json.dumps(args, default=str)})`:\n{content[:4000]}"
                for name, args, content in evidence
            )
        else:
            ev_text = "(no tool results were gathered)"

        synth_messages = [
            SystemMessage(content=SYSTEM_PROMPT),
            HumanMessage(
                content=(
                    f"{request_text}\n\n"
                    "You have stopped searching. Below is ALL the grounded context you "
                    "already retrieved from the sempkg tools. Using ONLY this evidence, "
                    "produce your final answer NOW as a `context_result` (kind="
                    "'context_result'). Do NOT ask a clarification and do NOT request more "
                    "tools. Populate `findings` from the evidence with exact files and line "
                    "ranges. If the evidence is genuinely insufficient, return a "
                    "context_result whose summary says so.\n\n"
                    f"=== GATHERED EVIDENCE ===\n{ev_text}"
                )
            ),
        ]
        structured = model.with_structured_output(AgentAnswer)
        # Reuse the run's callbacks (streaming + tracing) so the synthesis step is
        # visible too.
        invoke_cfg = {"callbacks": config["callbacks"]} if config.get("callbacks") else None
        answer = await structured.ainvoke(synth_messages, config=invoke_cfg)
        return _coerce_to_result(answer)

    async def aclose(self) -> None:
        """Release the persistent sempkg MCP session (warm subprocess)."""
        await self._tool_provider.aclose()

    def _postprocess(self, answer: AgentAnswer, request: ContextRequest) -> AgentAnswer:
        """Apply server-side guardrails (e.g. cap findings)."""
        if answer.is_clarification():
            return answer
        cap = request.max_findings or self._settings.agent.max_findings
        if len(answer.findings) > cap:
            logger.info("Trimming findings %d -> %d", len(answer.findings), cap)
            answer = answer.model_copy(update={"findings": answer.findings[:cap]})
        return answer

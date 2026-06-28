"""The knowledge agent: a LangGraph ReAct loop over the sempkg MCP tools.

The agent is provider-agnostic (any OpenAI-compatible / OpenRouter model) and
emits a single structured ``AgentAnswer`` per turn. Multi-turn clarification is
supported via a LangGraph checkpointer keyed by ``session_id``: when the caller
replies to a clarifying question with the same session id, the full conversation
history is replayed so the agent continues where it left off.
"""

from __future__ import annotations

import json
import logging
import uuid

from langchain_core.messages import AIMessage, HumanMessage, SystemMessage, ToolMessage
from langchain_openai import ChatOpenAI
from langgraph.checkpoint.memory import MemorySaver
from langgraph.errors import GraphRecursionError
from langgraph.prebuilt import create_react_agent

from .config import Settings
from .mcp_tools import SempkgToolProvider
from .prompts import SYSTEM_PROMPT, build_user_message
from .schemas import AgentAnswer, ContextRequest
from .tracing import AgentTracer

logger = logging.getLogger(__name__)


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
        self._graph = None  # built lazily in setup()
        self._model: ChatOpenAI | None = None

    @classmethod
    async def create(cls, settings: Settings) -> KnowledgeAgent:
        """Construct and fully initialise an agent (loads MCP tools + builds graph)."""
        agent = cls(settings, SempkgToolProvider(settings.mcp))
        await agent.setup()
        return agent

    async def setup(self) -> None:
        """Build the model + ReAct graph. Idempotent."""
        if self._graph is not None:
            return

        tools = await self._tool_provider.load()
        self._model = self._build_model()
        # MemorySaver keeps per-session conversation state in-process. For a
        # horizontally-scaled deployment, swap in a persistent checkpointer
        # (e.g. langgraph's Postgres/Redis saver) — the interface is identical.
        self._graph = create_react_agent(
            self._model,
            tools,
            prompt=SYSTEM_PROMPT,
            response_format=AgentAnswer,
            checkpointer=MemorySaver(),
        )
        logger.info("KnowledgeAgent ready (model=%s)", self._settings.llm.model)

    def _build_model(self) -> ChatOpenAI:
        llm = self._settings.llm
        if not llm.api_key_value:
            # Fail fast with a clear message rather than a cryptic 401 mid-request.
            raise RuntimeError(
                "No LLM API key configured. Set OPENROUTER_API_KEY (or "
                "SEMPKG_AGENT_API_KEY) before starting the agent server."
            )
        return ChatOpenAI(
            model=llm.model,
            base_url=llm.api_base,
            api_key=llm.api_key_value,
            temperature=llm.temperature,
            max_tokens=llm.max_tokens,
            timeout=llm.request_timeout,
            max_retries=llm.max_retries,
        )

    async def ask(self, request: ContextRequest) -> AgentAnswer:
        """Run one turn and return the structured answer.

        ``request.session_id`` ties multi-turn clarification together; when
        omitted a fresh, single-shot session id is generated.
        """
        if self._graph is None:
            raise RuntimeError("KnowledgeAgent.setup() must be called before ask().")

        thread_id = request.session_id or f"oneshot-{uuid.uuid4().hex}"
        user_text = build_user_message(request.prompt, request.package)
        recursion_limit = self._settings.agent.max_iterations * 2 + 5

        logger.info(
            "ask: thread=%s package=%s prompt=%r",
            thread_id,
            request.package,
            request.prompt[:200],
        )

        config: dict = {
            "configurable": {"thread_id": thread_id},
            "recursion_limit": recursion_limit,
        }
        if self._settings.agent.trace:
            # Logs LLM reasoning + every sempkg tool call/result for inspection.
            config["callbacks"] = [AgentTracer()]

        try:
            state = await self._graph.ainvoke(
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
            answer = await self._synthesize_from_state(config, user_text)

        if not isinstance(answer, AgentAnswer):
            raise RuntimeError(
                "Agent did not produce a structured answer; the model may not support "
                "structured output. Try a different SEMPKG_AGENT_MODEL."
            )

        return self._postprocess(answer, request)

    async def _synthesize_from_state(self, config: dict, request_text: str) -> AgentAnswer:
        """Force a final grounded answer from the context gathered so far.

        Used when the ReAct loop fails to terminate. Rather than replaying the raw
        tool-call conversation (which confuses weaker models and risks provider
        tool-pairing errors), we extract the gathered tool RESULTS into a clean
        evidence block and ask for a one-shot ``context_result``. The fallback
        never asks a clarification — it answers with whatever was found.
        """
        assert self._model is not None
        snapshot = await self._graph.aget_state(config)
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
        structured = self._model.with_structured_output(AgentAnswer)
        invoke_cfg = {"callbacks": [AgentTracer()]} if self._settings.agent.trace else None
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

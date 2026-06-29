"""The knowledge agent: a LangGraph ReAct loop over the sempkg MCP tools.

The agent is provider-agnostic (any OpenAI-compatible / OpenRouter model) and
emits a single structured ``AgentAnswer`` per turn. Multi-turn clarification is
supported via a LangGraph checkpointer keyed by ``session_id``: when the caller
replies to a clarifying question with the same session id, the full conversation
history is replayed so the agent continues where it left off.
"""

from __future__ import annotations

import asyncio
import contextlib
import json
import logging
import uuid
from collections.abc import AsyncIterator

from langchain_core.messages import AIMessage, HumanMessage, SystemMessage, ToolMessage
from langchain_core.tools import BaseTool
from langchain_openai import ChatOpenAI
from langgraph.checkpoint.memory import MemorySaver
from langgraph.errors import GraphRecursionError
from langgraph.prebuilt import create_react_agent

from .config import Settings
from .mcp_tools import SempkgToolProvider
from .models import is_allowed
from .prompts import build_user_message, system_prompt_for
from .render import render_result_markdown
from .schemas import AgentAnswer, ContextRequest
from .streaming import StreamCallback
from .tracing import AgentTracer
from .verify import verify_findings

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
        self._tools: list[BaseTool] | None = None  # loaded in setup()
        self._checkpointer = None  # shared across per-model graphs
        self._checkpointer_stack = contextlib.AsyncExitStack()
        # One ReAct graph per model id (tools + checkpointer + prompt shared).
        self._graphs: dict[str, object] = {}
        self._models: dict[str, ChatOpenAI] = {}
        self._default_model_name = settings.llm.model
        # System prompt: a host-supplied custom prompt if configured, else the
        # built-in persona prompt for the mode (human vs agent).
        self._system_prompt = system_prompt_for(
            settings.agent.mode,
            settings.agent.ui_title,
            settings.agent.system_prompt,
        )
        self._installed_text: str | None = None  # cached list_packages output

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
        self._checkpointer = await self._build_checkpointer()
        self._ensure_api_key()
        self._graph_for(self._default_model_name)  # warm the default
        logger.info(
            "KnowledgeAgent ready (mode=%s, default model=%s)",
            self._settings.agent.mode,
            self._default_model_name,
        )

    async def _build_checkpointer(self):
        """Pick the conversation-state store.

        Default is an in-process ``MemorySaver`` (state lost on restart). When
        ``SEMPKG_AGENT_STATE_DB`` is set we use a persistent SQLite checkpointer so
        multi-turn sessions survive restarts — important for a long-lived hosted
        human assistant. The saver requires the optional
        ``langgraph-checkpoint-sqlite`` package; we fall back to memory if absent.
        """
        db_path = self._settings.agent.state_db
        if not db_path:
            return MemorySaver()
        try:
            from langgraph.checkpoint.sqlite.aio import AsyncSqliteSaver
        except ImportError:
            logger.warning(
                "SEMPKG_AGENT_STATE_DB is set but langgraph-checkpoint-sqlite is not "
                "installed; falling back to in-memory state. Install the '[persist]' extra."
            )
            return MemorySaver()
        saver = await self._checkpointer_stack.enter_async_context(
            AsyncSqliteSaver.from_conn_string(db_path)
        )
        logger.info("Using persistent conversation state at %s", db_path)
        return saver

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
                prompt=self._system_prompt,
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
        user_text = build_user_message(request.prompt, request.package, request.version)

        logger.info(
            "ask: thread=%s model=%s package=%s version=%s prompt=%r",
            thread_id,
            model_name,
            request.package,
            request.version,
            request.prompt[:200],
        )

        config = self._make_config(thread_id)
        answer = await self._invoke(graph, model, config, user_text)
        return await self._finalize(answer, request, graph, config)

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
        user_text = build_user_message(request.prompt, request.package, request.version)
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
                outcome["answer"] = await self._finalize(answer, request, graph, config)
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
                    "answer": result.answer or result.summary,
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
            SystemMessage(content=self._system_prompt),
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
        """Release the warm sempkg MCP session and any persistent state store."""
        await self._tool_provider.aclose()
        with contextlib.suppress(Exception):
            await self._checkpointer_stack.aclose()

    async def _finalize(
        self, answer: AgentAnswer, request: ContextRequest, graph, config: dict
    ) -> AgentAnswer:
        """Cap findings and run the deterministic citation check before returning."""
        if answer.is_clarification():
            return answer
        answer = self._postprocess(answer, request)
        if self._settings.agent.verify_citations and answer.findings:
            answer = await self._verify_citations(answer, graph, config)
        return answer

    async def _verify_citations(self, answer: AgentAnswer, graph, config: dict) -> AgentAnswer:
        """Mark each finding verified/unverified against the retrieved evidence."""
        try:
            snapshot = await graph.aget_state(config)
            messages = list(snapshot.values.get("messages", []))
            evidence_texts = [content for _, _, content in _collect_tool_evidence(messages)]
            verified = verify_findings(answer.findings, evidence_texts)
            n_bad = sum(1 for f in verified if f.verified is False)
            if n_bad:
                logger.warning("%d/%d findings could not be verified.", n_bad, len(verified))
            return answer.model_copy(update={"findings": verified})
        except Exception:  # noqa: BLE001 - verification is best-effort, never fatal
            logger.exception("Citation verification failed; returning unverified findings")
            return answer

    def _postprocess(self, answer: AgentAnswer, request: ContextRequest) -> AgentAnswer:
        """Apply server-side guardrails (e.g. cap findings)."""
        if answer.is_clarification():
            return answer
        cap = request.max_findings or self._settings.agent.max_findings
        if len(answer.findings) > cap:
            logger.info("Trimming findings %d -> %d", len(answer.findings), cap)
            answer = answer.model_copy(update={"findings": answer.findings[:cap]})
        return answer

    # -- introspection helpers (used by the chat UI + raw MCP passthrough) --------

    def raw_tools(self) -> list[BaseTool]:
        """The underlying sempkg MCP tools (for re-exposing over the MCP mount)."""
        return list(self._tools or [])

    async def call_tool(self, name: str, arguments: dict) -> str:
        """Invoke one sempkg tool by name (passthrough for the MCP transport)."""
        tool = next((t for t in (self._tools or []) if t.name == name), None)
        if tool is None:
            raise KeyError(f"sempkg tool {name!r} is not available")
        return await tool.ainvoke(arguments)

    async def list_installed(self) -> str:
        """Return the sempkg `list_packages` output (cached). Empty string on failure."""
        if self._installed_text is None:
            try:
                self._installed_text = str(await self.call_tool("list_packages", {}))
            except Exception:  # noqa: BLE001 - non-fatal; UI degrades gracefully
                logger.exception("list_packages failed")
                self._installed_text = ""
        return self._installed_text

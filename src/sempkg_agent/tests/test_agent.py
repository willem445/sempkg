"""Orchestration-glue tests with a fake graph (no LLM, no MCP, no network)."""

from __future__ import annotations

import pytest
from langchain_core.messages import AIMessage, HumanMessage, ToolMessage
from langgraph.errors import GraphRecursionError

from sempkg_agent.agent import KnowledgeAgent, _coerce_to_result, _collect_tool_evidence
from sempkg_agent.config import Settings
from sempkg_agent.schemas import AgentAnswer, ContextRequest, Finding


class _FakeGraph:
    """Stands in for the LangGraph ReAct graph; records the invoke config."""

    def __init__(self, answer: AgentAnswer) -> None:
        self._answer = answer
        self.last_config = None

    async def ainvoke(self, state, config):
        self.last_config = config
        return {"structured_response": self._answer}


def _install(agent: KnowledgeAgent, graph, model=None) -> KnowledgeAgent:
    """Inject a fake graph/model for the default model id, bypassing setup()."""
    agent._tools = []  # marks setup() as done
    name = agent._default_model_name
    agent._graphs[name] = graph
    agent._models[name] = model if model is not None else object()
    return agent


def _agent_with(answer: AgentAnswer, **agent_overrides) -> tuple[KnowledgeAgent, _FakeGraph]:
    settings = Settings()
    for k, v in agent_overrides.items():
        setattr(settings.agent, k, v)
    agent = KnowledgeAgent(settings, tool_provider=None)  # provider unused in ask()
    graph = _FakeGraph(answer)
    _install(agent, graph)
    return agent, graph


def _finding(i: int) -> Finding:
    return Finding(
        package="p",
        file=f"f{i}.rs",
        start_line=i,
        end_line=i + 1,
        snippet="x",
        explanation="why",
    )


async def test_ask_returns_structured_answer() -> None:
    answer = AgentAnswer(
        kind="context_result", summary="s", reasoning="r", findings=[_finding(1)]
    )
    agent, _ = _agent_with(answer)
    out = await agent.ask(ContextRequest(prompt="hello"))
    assert out.kind == "context_result"
    assert out.summary == "s"


async def test_ask_uses_session_id_as_thread() -> None:
    answer = AgentAnswer(kind="context_result", summary="s", reasoning="r")
    agent, graph = _agent_with(answer)
    await agent.ask(ContextRequest(prompt="hi", session_id="sess-42"))
    assert graph.last_config["configurable"]["thread_id"] == "sess-42"


async def test_ask_generates_oneshot_thread_when_no_session() -> None:
    answer = AgentAnswer(kind="context_result", summary="s", reasoning="r")
    agent, graph = _agent_with(answer)
    await agent.ask(ContextRequest(prompt="hi"))
    assert graph.last_config["configurable"]["thread_id"].startswith("oneshot-")


async def test_findings_are_capped() -> None:
    answer = AgentAnswer(
        kind="context_result",
        summary="s",
        reasoning="r",
        findings=[_finding(i) for i in range(20)],
    )
    agent, _ = _agent_with(answer, max_findings=5)
    out = await agent.ask(ContextRequest(prompt="hi"))
    assert len(out.findings) == 5


async def test_clarification_not_capped_or_altered() -> None:
    answer = AgentAnswer(
        kind="clarification",
        clarifying_question="which package?",
        clarification_rationale="ambiguous",
    )
    agent, _ = _agent_with(answer)
    out = await agent.ask(ContextRequest(prompt="vague"))
    assert out.is_clarification()
    assert out.clarifying_question == "which package?"


async def test_streaming_emits_status_final_and_done() -> None:
    answer = AgentAnswer(
        kind="context_result", summary="streamed", reasoning="r", findings=[_finding(1)]
    )
    agent, _ = _agent_with(answer)
    events = [ev async for ev in agent.astream(ContextRequest(prompt="hi"))]
    types = [e["type"] for e in events]
    assert types[0] == "status"
    assert types[-1] == "done"
    final = next(e for e in events if e["type"] == "final")
    assert final["result"]["summary"] == "streamed"


async def test_streaming_clarification_event() -> None:
    answer = AgentAnswer(
        kind="clarification", clarifying_question="which pkg?", clarification_rationale="x"
    )
    agent, _ = _agent_with(answer)
    events = [ev async for ev in agent.astream(ContextRequest(prompt="vague"))]
    clar = next(e for e in events if e["type"] == "clarification")
    assert clar["question"] == "which pkg?"


async def test_ask_before_setup_raises() -> None:
    agent = KnowledgeAgent(Settings(), tool_provider=None)
    with pytest.raises(RuntimeError):
        await agent.ask(ContextRequest(prompt="hi"))


# --- recursion-limit fallback ------------------------------------------------


class _RecursionGraph:
    """Raises GraphRecursionError on ainvoke; exposes persisted messages."""

    def __init__(self, messages: list) -> None:
        self._messages = messages

    async def ainvoke(self, state, config):
        raise GraphRecursionError("loop did not converge")

    async def aget_state(self, config):
        class _Snap:
            pass

        snap = _Snap()
        snap.values = {"messages": self._messages}
        return snap


class _FakeStructured:
    def __init__(self, answer: AgentAnswer) -> None:
        self._answer = answer
        self.last_messages = None

    async def ainvoke(self, messages, config=None):
        self.last_messages = messages
        return self._answer


class _FakeModel:
    def __init__(self, answer: AgentAnswer) -> None:
        self.structured = _FakeStructured(answer)

    def with_structured_output(self, schema):
        return self.structured


async def test_recursion_limit_triggers_synthesis() -> None:
    synth = AgentAnswer(kind="context_result", summary="synthesized", reasoning="from history")
    agent = KnowledgeAgent(Settings(), tool_provider=None)
    model = _FakeModel(synth)
    _install(agent, _RecursionGraph([HumanMessage(content="hi")]), model=model)
    out = await agent.ask(ContextRequest(prompt="x"))
    assert out.kind == "context_result"
    assert out.summary == "synthesized"
    # The synthesis prompt presents the gathered evidence and forbids clarification.
    assert "GATHERED EVIDENCE" in model.structured.last_messages[-1].content


async def test_synthesis_coerces_stray_clarification() -> None:
    # Model returns a clarification even though we forced an answer -> coerce.
    stray = AgentAnswer(
        kind="clarification",
        clarifying_question="which one?",
        summary="actually found ExpansionKind",
        findings=[_finding(1)],
    )
    agent = KnowledgeAgent(Settings(), tool_provider=None)
    _install(agent, _RecursionGraph([HumanMessage(content="hi")]), model=_FakeModel(stray))
    out = await agent.ask(ContextRequest(prompt="x"))
    assert out.kind == "context_result"  # coerced
    assert len(out.findings) == 1  # findings preserved
    assert out.summary == "actually found ExpansionKind"


class _GraphWithState:
    """Returns a preset answer and exposes a tool-message history for verification."""

    def __init__(self, answer: AgentAnswer, messages: list) -> None:
        self._answer = answer
        self._messages = messages

    async def ainvoke(self, state, config):
        return {"structured_response": self._answer}

    async def aget_state(self, config):
        class _Snap:
            pass

        snap = _Snap()
        snap.values = {"messages": self._messages}
        return snap


async def test_citation_verification_marks_findings() -> None:
    snippet = "fn parse_variants(input: &str) -> Vec<Variant> {"
    answer = AgentAnswer(
        kind="context_result",
        summary="found it",
        reasoning="queried",
        findings=[
            Finding(package="p", file="f.rs", start_line=1, end_line=2,
                    snippet=snippet, explanation="why"),
            Finding(package="p", file="g.rs", start_line=9, end_line=9,
                    snippet="fn hallucinated_symbol() { nope() }", explanation="why"),
        ],
    )
    history = [
        HumanMessage(content="q"),
        AIMessage(content="", tool_calls=[{"name": "query", "args": {}, "id": "a"}]),
        ToolMessage(content=f"results...\n{snippet}\n    body\n}}", tool_call_id="a"),
    ]
    agent = KnowledgeAgent(Settings(), tool_provider=None)
    _install(agent, _GraphWithState(answer, history))
    out = await agent.ask(ContextRequest(prompt="x"))
    assert out.findings[0].verified is True   # snippet present in evidence
    assert out.findings[1].verified is False  # fabricated snippet flagged


async def test_default_mode_is_human() -> None:
    assert Settings().agent.mode == "human"


def test_collect_tool_evidence_pairs_calls_with_results() -> None:
    msgs = [
        HumanMessage(content="q"),
        AIMessage(content="", tool_calls=[{"name": "read_symbol", "args": {"s": "X"}, "id": "a"}]),
        ToolMessage(content="enum X {}", tool_call_id="a"),
        AIMessage(content="", tool_calls=[{"name": "query", "args": {}, "id": "b"}]),  # dangling
    ]
    ev = _collect_tool_evidence(msgs)
    assert ev == [("read_symbol", {"s": "X"}, "enum X {}")]  # dangling call yields no evidence


def test_coerce_to_result_passthrough_and_convert() -> None:
    res = AgentAnswer(kind="context_result", summary="s", reasoning="r")
    assert _coerce_to_result(res) is res
    clar = AgentAnswer(kind="clarification", clarifying_question="hm?")
    coerced = _coerce_to_result(clar)
    assert coerced.kind == "context_result"
    assert coerced.summary  # non-empty (falls back to the question/default)

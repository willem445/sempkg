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


def _agent_with(answer: AgentAnswer, **agent_overrides) -> KnowledgeAgent:
    settings = Settings()
    for k, v in agent_overrides.items():
        setattr(settings.agent, k, v)
    agent = KnowledgeAgent(settings, tool_provider=None)  # provider unused in ask()
    agent._graph = _FakeGraph(answer)
    return agent


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
    agent = _agent_with(answer)
    out = await agent.ask(ContextRequest(prompt="hello"))
    assert out.kind == "context_result"
    assert out.summary == "s"


async def test_ask_uses_session_id_as_thread() -> None:
    answer = AgentAnswer(kind="context_result", summary="s", reasoning="r")
    agent = _agent_with(answer)
    await agent.ask(ContextRequest(prompt="hi", session_id="sess-42"))
    assert agent._graph.last_config["configurable"]["thread_id"] == "sess-42"


async def test_ask_generates_oneshot_thread_when_no_session() -> None:
    answer = AgentAnswer(kind="context_result", summary="s", reasoning="r")
    agent = _agent_with(answer)
    await agent.ask(ContextRequest(prompt="hi"))
    assert agent._graph.last_config["configurable"]["thread_id"].startswith("oneshot-")


async def test_findings_are_capped() -> None:
    answer = AgentAnswer(
        kind="context_result",
        summary="s",
        reasoning="r",
        findings=[_finding(i) for i in range(20)],
    )
    agent = _agent_with(answer, max_findings=5)
    out = await agent.ask(ContextRequest(prompt="hi"))
    assert len(out.findings) == 5


async def test_clarification_not_capped_or_altered() -> None:
    answer = AgentAnswer(
        kind="clarification",
        clarifying_question="which package?",
        clarification_rationale="ambiguous",
    )
    agent = _agent_with(answer)
    out = await agent.ask(ContextRequest(prompt="vague"))
    assert out.is_clarification()
    assert out.clarifying_question == "which package?"


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
    agent._graph = _RecursionGraph([HumanMessage(content="hi")])
    agent._model = _FakeModel(synth)
    out = await agent.ask(ContextRequest(prompt="x"))
    assert out.kind == "context_result"
    assert out.summary == "synthesized"
    # The synthesis prompt presents the gathered evidence and forbids clarification.
    assert "GATHERED EVIDENCE" in agent._model.structured.last_messages[-1].content


async def test_synthesis_coerces_stray_clarification() -> None:
    # Model returns a clarification even though we forced an answer -> coerce.
    stray = AgentAnswer(
        kind="clarification",
        clarifying_question="which one?",
        summary="actually found ExpansionKind",
        findings=[_finding(1)],
    )
    agent = KnowledgeAgent(Settings(), tool_provider=None)
    agent._graph = _RecursionGraph([HumanMessage(content="hi")])
    agent._model = _FakeModel(stray)
    out = await agent.ask(ContextRequest(prompt="x"))
    assert out.kind == "context_result"  # coerced
    assert len(out.findings) == 1  # findings preserved
    assert out.summary == "actually found ExpansionKind"


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

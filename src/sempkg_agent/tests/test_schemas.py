"""Schema/contract tests (no network, no LLM)."""

from __future__ import annotations

import pytest
from pydantic import ValidationError

from sempkg_agent.schemas import AgentAnswer, Finding


def _finding(**kw) -> Finding:
    base = dict(
        package="lancedb",
        file="src/index.rs",
        start_line=10,
        end_line=20,
        snippet="fn open_bm25() {}",
        explanation="opens the BM25 index",
    )
    base.update(kw)
    return Finding(**base)


def test_clarification_requires_question() -> None:
    with pytest.raises(ValidationError):
        AgentAnswer(kind="clarification")


def test_context_result_requires_summary() -> None:
    with pytest.raises(ValidationError):
        AgentAnswer(kind="context_result")


def test_valid_clarification_roundtrip() -> None:
    a = AgentAnswer(
        kind="clarification",
        clarifying_question="Which package?",
        clarification_rationale="ambiguous symbol",
    )
    assert a.is_clarification()
    clar = a.as_clarification()
    assert clar.question == "Which package?"
    assert clar.rationale == "ambiguous symbol"


def test_valid_result_and_files_dedup() -> None:
    a = AgentAnswer(
        kind="context_result",
        summary="found it",
        reasoning="queried lancedb",
        packages_searched=["lancedb"],
        findings=[
            _finding(),
            _finding(start_line=30, end_line=40),  # same file -> one entry
            _finding(file="src/bm25.rs"),
        ],
    )
    assert not a.is_clarification()
    result = a.as_result()
    # Two distinct files, in first-seen order.
    assert result.files == ["lancedb:src/index.rs", "lancedb:src/bm25.rs"]

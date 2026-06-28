"""Markdown rendering tests (no network)."""

from __future__ import annotations

from sempkg_agent.render import render_clarification_markdown, render_result_markdown
from sempkg_agent.schemas import ClarificationRequest, ContextResult, Finding


def test_result_markdown_contains_grounding() -> None:
    result = ContextResult(
        summary="how merge works",
        reasoning="queried pandas",
        packages_searched=["pandas"],
        findings=[
            Finding(
                package="pandas",
                file="core/reshape/merge.py",
                start_line=120,
                end_line=180,
                kind="code",
                symbol="merge",
                snippet="def merge(...):\n    ...",
                explanation="defines the merge entry point",
            )
        ],
    )
    md = render_result_markdown(result)
    assert "Sources" in md
    assert "pandas" in md
    assert "core/reshape/merge.py:120-180" in md
    assert "def merge" in md
    assert "defines the merge entry point" in md


def test_result_markdown_leads_with_prose_answer() -> None:
    result = ContextResult(
        summary="short",
        answer="## Heading\n\nThe direct answer in **prose**.",
        reasoning="queried",
        packages_searched=["pandas"],
        findings=[
            Finding(
                package="pandas",
                version="v2.2",
                file="core/x.py",
                start_line=1,
                end_line=2,
                snippet="code",
                explanation="why",
                verified=True,
            )
        ],
    )
    md = render_result_markdown(result)
    # Prose answer leads; version + verified badge surface in the sources.
    assert md.startswith("## Heading")
    assert "pandas@v2.2" in md
    assert "verified" in md


def test_empty_findings_renders_notice() -> None:
    md = render_result_markdown(ContextResult(summary="nothing", reasoning="searched all"))
    assert "No grounded context" in md


def test_clarification_markdown() -> None:
    md = render_clarification_markdown(
        ClarificationRequest(question="Which version?", rationale="two are installed")
    )
    assert "Clarification needed" in md
    assert "Which version?" in md

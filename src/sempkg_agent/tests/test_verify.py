"""Deterministic citation-verification tests (no network)."""

from __future__ import annotations

from sempkg_agent.schemas import Finding
from sempkg_agent.verify import verify_findings


def _finding(snippet: str) -> Finding:
    return Finding(
        package="p", file="f.rs", start_line=1, end_line=3, snippet=snippet, explanation="why"
    )


def test_snippet_present_in_evidence_is_verified() -> None:
    evidence = ["... context ...\nfn parse_variants(input: &str) -> Vec<Variant> {\n    todo!()\n}"]
    [f] = verify_findings([_finding("fn parse_variants(input: &str) -> Vec<Variant> {")], evidence)
    assert f.verified is True


def test_fabricated_snippet_is_flagged_unverified() -> None:
    evidence = ["the retrieved code talks about merge and reshape"]
    [f] = verify_findings([_finding("fn totally_made_up_symbol() { secret() }")], evidence)
    assert f.verified is False


def test_whitespace_differences_are_tolerated() -> None:
    evidence = ["fn  parse_variants(input:   &str)  {\n\t  body\n}"]
    [f] = verify_findings([_finding("fn parse_variants(input: &str) {\n    body\n}")], evidence)
    assert f.verified is True


def test_no_evidence_leaves_flag_unset() -> None:
    [f] = verify_findings([_finding("anything")], [])
    assert f.verified is None

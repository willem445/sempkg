"""Deterministic citation verification.

The agent's value proposition is *grounded* answers: every cited snippet should
come from the installed bundles, not from the model's imagination. An LLM can,
however, lightly paraphrase a snippet or transpose a line number. This module
provides a cheap, deterministic check — no extra LLM or tool calls — that
confirms each finding's ``snippet`` actually appears in the evidence the agent
retrieved during the run.

A finding is marked ``verified=True`` when enough of its snippet's significant
lines are present (normalised for whitespace) in the gathered tool output,
``verified=False`` when it cannot be confirmed, leaving callers free to trust,
flag, or drop unconfirmed citations.
"""

from __future__ import annotations

import re

from .schemas import Finding

_WS = re.compile(r"\s+")


def _normalize(text: str) -> str:
    """Collapse all whitespace runs to single spaces and strip — tolerant matching."""
    return _WS.sub(" ", text).strip()


def _significant_lines(snippet: str) -> list[str]:
    """Normalised snippet lines worth matching (skip trivial/punctuation-only lines)."""
    lines: list[str] = []
    for raw in snippet.splitlines():
        norm = _normalize(raw)
        # Skip near-empty lines and bare delimiters (}, ), etc.) that match anywhere.
        if len(norm) >= 4 and any(c.isalnum() for c in norm):
            lines.append(norm)
    return lines


def verify_findings(
    findings: list[Finding],
    evidence_texts: list[str],
    threshold: float = 0.6,
) -> list[Finding]:
    """Return findings with ``verified`` set by checking snippets against evidence.

    ``evidence_texts`` is the raw output of every sempkg tool call made during the
    run (query results, read_code/read_docs/read_symbol bodies). A snippet counts
    as verified when at least ``threshold`` of its significant lines appear in that
    corpus. Empty/whitespace snippets are left as ``verified=None``.
    """
    if not findings:
        return findings

    corpus = _normalize("\n".join(evidence_texts))
    if not corpus:
        # No evidence to check against — leave the flag unset rather than failing all.
        return [f.model_copy(update={"verified": None}) for f in findings]

    out: list[Finding] = []
    for f in findings:
        lines = _significant_lines(f.snippet or "")
        if not lines:
            out.append(f.model_copy(update={"verified": None}))
            continue
        matched = sum(1 for ln in lines if ln in corpus)
        verified = (matched / len(lines)) >= threshold
        out.append(f.model_copy(update={"verified": verified}))
    return out

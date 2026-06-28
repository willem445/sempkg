"""Render structured answers into a compact, agent-friendly markdown view.

Every transport returns the structured JSON (the machine contract) AND this
markdown rendering (a human/agent-readable view), so callers can consume whichever
they prefer.
"""

from __future__ import annotations

from .schemas import ClarificationRequest, ContextResult


def render_result_markdown(result: ContextResult) -> str:
    """Render a grounded context result as markdown."""
    lines: list[str] = []
    lines.append("## Context result")
    lines.append("")
    lines.append(f"**Summary:** {result.summary}")
    lines.append("")
    if result.packages_searched:
        lines.append(f"**Packages searched:** {', '.join(result.packages_searched)}")
    if result.files:
        lines.append(f"**Files with relevant context:** {', '.join(result.files)}")
    lines.append("")
    lines.append(f"**Reasoning:** {result.reasoning}")
    lines.append("")

    if not result.findings:
        lines.append("_No grounded context was found for this request._")
        return "\n".join(lines)

    lines.append(f"### Findings ({len(result.findings)})")
    for i, f in enumerate(result.findings, start=1):
        loc = f.file
        if f.start_line:
            loc += f":{f.start_line}"
            if f.end_line and f.end_line != f.start_line:
                loc += f"-{f.end_line}"
        title = f"{i}. `{f.symbol}`" if f.symbol else f"{i}. `{loc}`"
        lines.append("")
        lines.append(f"#### {title}")
        lines.append("")
        lines.append(f"- **Package:** `{f.package}`")
        lines.append(f"- **Location:** `{loc}`  ({f.kind})")
        lines.append(f"- **Why:** {f.explanation}")
        lines.append("")
        lines.append("```")
        lines.append(f.snippet.rstrip("\n"))
        lines.append("```")
    return "\n".join(lines)


def render_clarification_markdown(clar: ClarificationRequest) -> str:
    """Render a clarification request as markdown."""
    return (
        "## Clarification needed\n\n"
        f"**Question:** {clar.question}\n\n"
        f"**Why:** {clar.rationale}"
    )

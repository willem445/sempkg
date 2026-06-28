"""Render structured answers into a compact, agent-friendly markdown view.

Every transport returns the structured JSON (the machine contract) AND this
markdown rendering (a human/agent-readable view), so callers can consume whichever
they prefer.
"""

from __future__ import annotations

from .schemas import ClarificationRequest, ContextResult


def _location(f) -> str:
    """`pkg@version path:line-range` for a finding."""
    pkg = f.package
    if f.version:
        pkg += f"@{f.version}"
    loc = f.file
    if f.start_line:
        loc += f":{f.start_line}"
        if f.end_line and f.end_line != f.start_line:
            loc += f"-{f.end_line}"
    return f"{pkg} {loc}"


def render_result_markdown(result: ContextResult) -> str:
    """Render a grounded context result as markdown.

    In human mode the model supplies a prose ``answer`` (Markdown); we lead with it
    and append a **Sources** section from the findings. In agent mode (no ``answer``)
    we fall back to a compact summary + findings listing.
    """
    lines: list[str] = []

    if result.answer:
        lines.append(result.answer.rstrip("\n"))
    else:
        lines.append(f"**Summary:** {result.summary}")
        lines.append("")
        lines.append(f"**Reasoning:** {result.reasoning}")

    if not result.findings:
        if not result.answer:
            lines.append("")
            lines.append("_No grounded context was found in the installed bundles._")
        return "\n".join(lines)

    lines.append("")
    lines.append(f"### Sources ({len(result.findings)})")
    for i, f in enumerate(result.findings, start=1):
        badge = ""
        if f.verified is True:
            badge = " ✓ verified"
        elif f.verified is False:
            badge = " ⚠ unverified"
        title = f.symbol or _location(f)
        lines.append("")
        lines.append(f"**{i}. `{title}`**{badge}")
        lines.append(f"`{_location(f)}` · {f.kind}")
        if f.explanation:
            lines.append(f"> {f.explanation}")
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

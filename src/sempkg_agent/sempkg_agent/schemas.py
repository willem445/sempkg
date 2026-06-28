"""Structured request/response contracts.

These models are the stable, transport-independent contract between the
orchestrator and every inbound protocol (A2A / MCP / REST). The agent is
constrained to emit exactly one ``AgentAnswer`` per turn, which is either a
clarification request or a grounded context result.
"""

from __future__ import annotations

from typing import Literal

from pydantic import BaseModel, Field, model_validator


class ContextRequest(BaseModel):
    """A caller's context-retrieval request."""

    prompt: str = Field(..., description="Natural-language description of the context needed.")
    package: str | None = Field(
        default=None,
        description=(
            "Optional package scope hint. When the caller is confident the context lives in a "
            "specific package (e.g. 'pandas'), the agent focuses there first but may still consult "
            "closely related packages if needed."
        ),
    )
    session_id: str | None = Field(
        default=None,
        description="Conversation/thread id for multi-turn clarification continuity.",
    )
    max_findings: int | None = Field(
        default=None, ge=1, le=50, description="Optional cap on returned findings."
    )


class Finding(BaseModel):
    """A single piece of grounded context selected for the caller."""

    package: str = Field(..., description="Package the context was found in.")
    file: str = Field(..., description="Source/doc file path within the package.")
    start_line: int = Field(
        ..., ge=0, description="1-based start line (0 if not line-addressable)."
    )
    end_line: int = Field(..., ge=0, description="1-based inclusive end line (0 if unknown).")
    kind: Literal["code", "docs", "symbol", "other"] = Field(
        default="code", description="Nature of the context."
    )
    symbol: str | None = Field(default=None, description="Symbol name when applicable.")
    snippet: str = Field(
        ..., description="The full relevant context text, verbatim from retrieval."
    )
    explanation: str = Field(
        ..., description="Brief reasoning for why this context fulfils the caller's request."
    )


class ContextResult(BaseModel):
    """The grounded answer payload returned to the caller."""

    kind: Literal["context_result"] = "context_result"
    summary: str = Field(..., description="Concise summary of the retrieved context.")
    reasoning: str = Field(
        ..., description="Overall reasoning: how the context was located and why it was selected."
    )
    packages_searched: list[str] = Field(
        default_factory=list, description="Packages queried while resolving the request."
    )
    findings: list[Finding] = Field(
        default_factory=list, description="Selected context, grouped per file/symbol."
    )

    @property
    def files(self) -> list[str]:
        """Distinct files containing relevant context, in first-seen order."""
        seen: dict[str, None] = {}
        for f in self.findings:
            seen.setdefault(f"{f.package}:{f.file}", None)
        return list(seen.keys())


class ClarificationRequest(BaseModel):
    """Emitted when the agent needs more information before it can retrieve well."""

    kind: Literal["clarification"] = "clarification"
    question: str = Field(..., description="The clarifying question to ask the calling agent.")
    rationale: str = Field(
        ..., description="Why the clarification is needed to retrieve the right context."
    )


class AgentAnswer(BaseModel):
    """The agent's single structured output per turn (discriminated by ``kind``).

    A flat model (rather than a typed union) keeps the schema friendly to LLM
    structured-output backends while still being unambiguous via ``kind``.
    """

    kind: Literal["context_result", "clarification"] = Field(
        ..., description="'clarification' to ask the caller a question; 'context_result' to answer."
    )

    # context_result fields
    summary: str | None = None
    reasoning: str | None = None
    packages_searched: list[str] = Field(default_factory=list)
    findings: list[Finding] = Field(default_factory=list)

    # clarification fields
    clarifying_question: str | None = None
    clarification_rationale: str | None = None

    @model_validator(mode="after")
    def _check_shape(self) -> AgentAnswer:
        if self.kind == "clarification":
            if not self.clarifying_question:
                raise ValueError("clarification answers require 'clarifying_question'")
        else:  # context_result
            if not self.summary:
                raise ValueError("context_result answers require 'summary'")
        return self

    def is_clarification(self) -> bool:
        return self.kind == "clarification"

    def as_result(self) -> ContextResult:
        return ContextResult(
            summary=self.summary or "",
            reasoning=self.reasoning or "",
            packages_searched=self.packages_searched,
            findings=self.findings,
        )

    def as_clarification(self) -> ClarificationRequest:
        return ClarificationRequest(
            question=self.clarifying_question or "",
            rationale=self.clarification_rationale or "",
        )

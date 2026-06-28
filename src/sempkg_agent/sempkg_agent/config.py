"""Runtime configuration for the sempkg agent server.

All settings are loaded from environment variables (prefixed ``SEMPKG_AGENT_``)
with sensible production defaults, so the same image runs locally, on-prem, or on
a hosted AWS service with config-only changes. A local ``.env`` file is honoured
for development convenience.

The LLM is provider-agnostic but defaults to an OpenAI-compatible endpoint
(OpenRouter), so any OpenRouter-hosted model can be selected purely via
``SEMPKG_AGENT_MODEL`` without code changes.
"""

from __future__ import annotations

import os
import shlex
from functools import lru_cache
from pathlib import Path
from typing import Literal

from pydantic import Field, SecretStr, field_validator, model_validator
from pydantic_settings import BaseSettings, SettingsConfigDict


class LLMSettings(BaseSettings):
    """LLM backend configuration (OpenAI-compatible; OpenRouter by default)."""

    model_config = SettingsConfigDict(
        env_prefix="SEMPKG_AGENT_",
        env_file=".env",
        extra="ignore",
    )

    # Model is fully configurable; default is a strong agentic-tool-use model.
    model: str = Field(
        default="anthropic/claude-3.5-sonnet",
        description="OpenRouter model slug (or any model the api_base serves).",
    )
    api_base: str = Field(
        default="https://openrouter.ai/api/v1",
        description="OpenAI-compatible base URL for chat completions.",
    )
    # SEMPKG_AGENT_API_KEY (via env_prefix) takes precedence; otherwise we fall
    # back to OPENROUTER_API_KEY, which the sempkg reranker subprocess also reads.
    api_key: SecretStr | None = Field(default=None)
    temperature: float = Field(default=0.0, ge=0.0, le=2.0)
    max_tokens: int = Field(default=4096, gt=0)
    request_timeout: float = Field(default=120.0, gt=0)
    max_retries: int = Field(default=3, ge=0)

    @model_validator(mode="after")
    def _fallback_api_key(self) -> LLMSettings:
        if self.api_key is None:
            env_key = os.environ.get("OPENROUTER_API_KEY")
            if env_key:
                self.api_key = SecretStr(env_key)
        return self

    @property
    def api_key_value(self) -> str | None:
        return self.api_key.get_secret_value() if self.api_key else None


class MCPSettings(BaseSettings):
    """How to launch and talk to the local ``sempkg`` MCP server (retrieval)."""

    model_config = SettingsConfigDict(
        env_prefix="SEMPKG_AGENT_MCP_",
        env_file=".env",
        extra="ignore",
    )

    command: str = Field(
        default="sempkg",
        description="Executable that launches the sempkg MCP server.",
    )
    # Extra args appended after the resolved ``mcp -C <workspace>`` invocation.
    extra_args: str = Field(default="", description="Additional CLI args (shell-split).")
    workspace: str = Field(
        default=".",
        description="Workspace dir (passed to `sempkg mcp -C`) holding sempkg.toml + bundles.",
    )
    # Curated, query-first tool surface exposed to the agent. Empty -> all tools.
    allowed_tools: list[str] = Field(
        default_factory=lambda: [
            "query",
            "list_packages",
            "list_files",
            "read_code",
            "read_docs",
            "read_symbol",
            "get_callers",
            "get_callees",
            "get_impact",
        ]
    )
    startup_timeout: float = Field(default=60.0, gt=0)

    @field_validator("allowed_tools", mode="before")
    @classmethod
    def _split_csv(cls, v: object) -> object:
        if isinstance(v, str):
            return [t.strip() for t in v.split(",") if t.strip()]
        return v

    def argv(self) -> list[str]:
        """Full argument vector (excluding the command itself)."""
        args = ["mcp", "-C", self.workspace]
        if self.extra_args:
            args.extend(shlex.split(self.extra_args))
        return args


class AgentSettings(BaseSettings):
    """Orchestration policy for the retrieval/answer loop."""

    model_config = SettingsConfigDict(
        env_prefix="SEMPKG_AGENT_",
        env_file=".env",
        extra="ignore",
    )

    # Persona/output mode — selects the *built-in* default system prompt:
    #   "human" — a knowledge assistant for people: prose answers (Markdown) that
    #             cite sources with code snippets; says "I don't know" when the
    #             answer isn't in the installed bundles. This is the default.
    #   "agent" — a machine-to-machine retrieval agent: compact, structured findings
    #             optimised for a *calling agent* rather than a person.
    mode: Literal["human", "agent"] = Field(default="human")
    # Bring-your-own system prompt. When set, it REPLACES the built-in persona
    # prompt entirely, so any deployment can define its own assistant behaviour
    # (the agent in front of code + docs is just the default use case). Provide the
    # text directly via SEMPKG_AGENT_SYSTEM_PROMPT, or a path via
    # SEMPKG_AGENT_SYSTEM_PROMPT_FILE (loaded at startup).
    system_prompt: str | None = Field(default=None)
    system_prompt_file: str | None = Field(default=None)
    # Chat-UI branding — generic by default; name it for your deployment.
    ui_title: str = Field(default="sempkg knowledge agent")
    ui_subtitle: str = Field(
        default="Ask about the installed code and docs — grounded, with cited sources."
    )
    # Hard ceiling on agent tool-call rounds — a cost-control guardrail.
    max_iterations: int = Field(default=12, gt=0)
    # Default number of hits requested from the `query` tool per search.
    default_query_limit: int = Field(default=10, gt=0)
    # Max findings returned to the caller (keeps the payload focused).
    max_findings: int = Field(default=12, gt=0)
    # Deterministic citation check: confirm each finding's snippet actually appears
    # in the retrieved evidence (no extra LLM/tool calls). Flags hallucinated cites.
    verify_citations: bool = Field(default=True)
    # When true, log every LLM reasoning step + sempkg tool call/result
    # (to the `sempkg_agent.trace` logger). Off by default; enable for inspection.
    trace: bool = Field(default=False)
    # Optional path to a SQLite file for persistent conversation state. When set,
    # multi-turn sessions survive restarts (vs the default in-process MemorySaver).
    state_db: str | None = Field(default=None)

    @model_validator(mode="after")
    def _load_prompt_file(self) -> AgentSettings:
        """Load a custom system prompt from file when a path was given."""
        if self.system_prompt is None and self.system_prompt_file:
            path = Path(self.system_prompt_file).expanduser()
            try:
                self.system_prompt = path.read_text(encoding="utf-8")
            except OSError as exc:  # surface clearly rather than silently ignoring
                raise ValueError(
                    f"SEMPKG_AGENT_SYSTEM_PROMPT_FILE could not be read: {path} ({exc})"
                ) from exc
        return self


class ServerSettings(BaseSettings):
    """Network/server-level configuration shared by all transports."""

    model_config = SettingsConfigDict(
        env_prefix="SEMPKG_AGENT_",
        env_file=".env",
        extra="ignore",
    )

    host: str = Field(default="0.0.0.0")
    port: int = Field(default=8900, gt=0, lt=65536)
    log_level: str = Field(default="INFO")
    # Public URL the A2A AgentCard advertises (must be reachable by callers).
    public_url: str = Field(default="http://localhost:8900")
    # Optional shared-secret bearer token gating inbound requests. Empty = open.
    auth_token: SecretStr | None = Field(default=None)

    @property
    def auth_token_value(self) -> str | None:
        return self.auth_token.get_secret_value() if self.auth_token else None


class Settings(BaseSettings):
    """Top-level settings aggregate."""

    model_config = SettingsConfigDict(extra="ignore")

    llm: LLMSettings = Field(default_factory=LLMSettings)
    mcp: MCPSettings = Field(default_factory=MCPSettings)
    agent: AgentSettings = Field(default_factory=AgentSettings)
    server: ServerSettings = Field(default_factory=ServerSettings)


@lru_cache(maxsize=1)
def get_settings() -> Settings:
    """Return the process-wide settings singleton."""
    return Settings()

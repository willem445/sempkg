# sempkg-agent — Knowledge Agent Server Prototype

> **Status:** Implemented (prototype) · **Date:** 2026-06-27
> **Code:** [`src/sempkg_agent/`](../../src/sempkg_agent/)
> **Related:** [plan-knowledge-agent-server.md](../plans/plan-knowledge-agent-server.md)

This is the local-runnable first cut of the "active knowledge service" vision: an
agent that receives a request from a calling agent, retrieves version-accurate
context from installed sembundles, and returns exactly the needed context in a
machine-consumable form — with conversational clarification when needed.

It implements **Phase 0–2 + parts of Phase 5** of the plan (prove the loop,
LLM-synthesised grounded answers, and multi-protocol exposure), against the
*installed-workspace* retrieval path rather than the hosted bundle cache.

## What it does

1. A calling agent sends a natural-language request (optionally scoped to a package).
2. A LangGraph ReAct agent runs the retrieval policy:
   - `query` first — deep, reranked search across **all** installed packages, or
     scoped to one when the caller is confident;
   - under an explicit scope, it may also consult **closely related** packages
     (confirmed via `list_packages`) when needed;
   - drills into hits with `read_code` / `read_symbol` / `read_docs` to capture
     full snippets and exact line ranges.
3. It returns a single structured `AgentAnswer`:
   - `context_result`: `summary`, `reasoning`, `packages_searched`, and per-finding
     `package` / `file` / `start_line`–`end_line` / `snippet` / `explanation`; or
   - `clarification`: a question for the caller (ambiguous request).

## Protocol decision: A2A primary, REST + MCP alongside

The defining requirement is **conversational back-and-forth** — the agent may ask
the caller a clarifying question and wait. A2A models this natively with its task
lifecycle `input-required` state; the caller replies on the same `contextId`,
which we map onto the agent's LangGraph `session_id` so history replays. MCP
(host→tool) has no clean server-asks-caller turn, so it is secondary.

| Transport | Role | Multi-turn |
|-----------|------|-----------|
| **A2A** | primary agent-to-agent surface (`retrieve_package_context` skill) | ✅ `input-required` |
| **REST** | `POST /v1/ask` for curl/manual testing | ✅ via `session_id` |
| **MCP mount** | network `ask` tool for MCP-native hosts | ✅ via `session_id` |

All three wrap one orchestrator (`KnowledgeAgent.ask`) — "one capability, three
transports".

## Key components

| File | Responsibility |
|------|----------------|
| `config.py` | env-driven settings (model, MCP launch, guardrails, server) |
| `schemas.py` | the structured `AgentAnswer` / `ContextResult` / `Finding` contract |
| `prompts.py` | system prompt encoding the retrieval + clarification policy |
| `mcp_tools.py` | mounts the local `sempkg` MCP server's tools as LangChain tools |
| `agent.py` | LangGraph ReAct loop + structured output + per-session memory |
| `a2a_server.py` / `a2a_executor.py` | A2A AgentCard + task lifecycle bridge |
| `rest.py` | FastAPI REST transport + SSE streaming + chat UI + `/v1/models` |
| `mcp_server.py` | network MCP `ask` tool transport |
| `streaming.py` | queue-backed callback turning the agent loop into live events |
| `models.py` | curated, tiered OpenRouter model catalog (per-request selection) |
| `static/index.html` | no-build chat UI (answer + live activity toggle + model dropdown) |
| `cli.py` | `serve --transport {a2a,rest,chat,mcp,all}` |

## Chat UI

`--transport chat` (or `all`) serves a copilot-style chat at `/`: question →
grounded answer (summary + findings), a **"show activity" toggle** that streams the
agent's reasoning and every sempkg tool call/result live over SSE
(`POST /v1/ask/stream`), and a **model dropdown** (`GET /v1/models`) routing to a
curated, tiered set of OpenRouter models. Per-request model selection is validated
against the catalog so callers can't route arbitrary expensive models.

## Human knowledge-agent mode (default)

The agent runs in one of two personas (`SEMPKG_AGENT_MODE`, default `human`):

- **human** — an assistant in front of installed code + docs. It returns a prose
  Markdown `answer` (the body a person reads) plus structured `findings` as cited
  sources, and is disciplined to say "I don't know" rather than answer from general
  knowledge when the bundles don't cover it. The chat UI renders the Markdown answer
  and source cards (with `package@version`, line ranges, and a ✓/⚠ citation-check
  badge). This is the "team agent in front of code + docs" use case.
- **agent** — the original machine-to-machine persona: compact structured findings
  for a *calling agent*.

Any deployment can replace the built-in prompt wholesale with
`SEMPKG_AGENT_SYSTEM_PROMPT` / `SEMPKG_AGENT_SYSTEM_PROMPT_FILE` (the code+docs
assistant is only the default), and brand the UI via `SEMPKG_AGENT_UI_TITLE` /
`SEMPKG_AGENT_UI_SUBTITLE` (surfaced through `GET /v1/config`).

**Recommended improvements now built in:**
- *Deterministic citation verification* (`verify.py`) — each finding's snippet is
  checked against the retrieved evidence (no extra LLM/tool calls) and flagged
  `verified` true/false, turning "grounded" from a prompt instruction into an
  enforced property.
- *Persistent conversation state* — `SEMPKG_AGENT_STATE_DB` swaps the in-process
  `MemorySaver` for a SQLite checkpointer so sessions survive restarts.

## Raw MCP passthrough (for calling agents)

The MCP mount (`--transport mcp`, port `+2` under `all`) now re-exports the
**standard sempkg retrieval tools** (`query`, `read_code`, `read_docs`,
`read_symbol`, the call-graph tools) over streamable-HTTP — in addition to the
high-level `ask` tool — by proxying the agent's warm sempkg session. This gives the
"just expose MCP" path (a capable agent drives retrieval itself) and the
"distilled answer" path from one hosted process, against the same curated bundles.

## Versioned retrieval — open core dependency

To let users ask both "how does X work?" (latest) and "in v14.2.0, how did X work?"
(a specific release), multiple versions of the same bundle may be installed at once.
The agent forwards a `version` scope (UI release box / REST+MCP `version` field) and
the prompt favours the latest by default — but the underlying `query`/`search_*`
tools don't yet accept a `version` filter, so a raw query spans **all** installed
versions. Clean versioned retrieval needs two things in the sempkg core query layer:

1. **default-to-latest** — with no filter, search only the newest installed version
   of each package; and
2. a **`version` filter** argument honoured by `query`/`search_*`.

Until then, the release scope is a soft hint. Interim options (single-version
install, or distinct package names per release) are documented in
[`deploy/README.md`](../../deploy/README.md).

## Deliberately deferred (see plan doc)

- **Bundle cache (hot/cold tiering)** and the **package/version router** — this
  prototype serves whatever the configured workspace has installed (plan §4–§6).
- **`sempkg serve` (Axum) retrieval contract** — we use the existing stdio MCP
  server for now (plan §4, Option A vs B).
- **Persistent conversation state** — in-process `MemorySaver`; swap a
  Postgres/Redis checkpointer for horizontal scale.
- **Observability / eval / rate-limiting** — hooks noted in the README (plan §7–§8).

## Cost & safety guardrails

- `MAX_ITERATIONS` (tool-call rounds) and `MAX_FINDINGS` bound work per request.
- Retrieved content is treated as untrusted data in the prompt (injection hardening).
- Version isolation: the prompt forbids blending across versions of a package.
- The model is fully configurable (`SEMPKG_AGENT_MODEL`) — start cheap while monitoring.

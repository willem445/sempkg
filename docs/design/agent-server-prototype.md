# sempkg-agent — Knowledge Agent Server Prototype

> **Status:** Implemented (prototype) · **Date:** 2026-06-27
> **Code:** [`src/sempkg_agent/`](../../src/sempkg_agent/)
> **Related:** [plan-knowledge-agent-server.md](../plan-knowledge-agent-server.md)

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

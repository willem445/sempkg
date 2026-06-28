# sempkg-agent

A grounded **code-intelligence agent server** that sits in front of a workspace
of installed `.sembundle` packages. A *calling agent* sends a natural-language
request; sempkg-agent performs version-accurate retrieval over the installed
bundles (via the local `sempkg` MCP server) and returns **exactly the context the
caller needs** — which package it came from, the files and line ranges, verbatim
snippets, the reasoning behind the selection, and a summary. When a request is
ambiguous it can ask the caller a clarifying question and continue the
conversation.

This is the local-runnable precursor to the hosted "knowledge agent" described in
[`docs/plan-knowledge-agent-server.md`](../../docs/plan-knowledge-agent-server.md).

---

## Architecture

```
 calling agent ──▶  Transport          ──▶  KnowledgeAgent (LangGraph ReAct)
 (A2A / MCP / REST)                          │
                                             ▼
                                     sempkg MCP tools  (query, read_code, …)
                                             │
                                             ▼
                                   CodeGraph + QMD/LanceDB indexes
                                   (version-pinned, per installed bundle)
                                             │
                                             ▼
                          LLM backend (OpenRouter / any OpenAI-compatible)
```

- **Orchestration:** LangChain + LangGraph ReAct agent (`agent.py`). The model is
  fully configurable (`SEMPKG_AGENT_MODEL`), routed through an OpenAI-compatible
  endpoint (OpenRouter by default).
- **Retrieval:** the agent calls the local `sempkg` MCP server's tools
  (`query` first, then `read_code` / `read_symbol` / `read_docs` and the call-graph
  tools) via `langchain-mcp-adapters`. All results are version-accurate and grounded.
- **Output contract:** a single structured `AgentAnswer` per turn — either a
  `context_result` (summary + reasoning + per-finding package/file/line/snippet/why)
  or a `clarification` (a question for the caller). See `schemas.py`.

### Why A2A is the primary protocol

The agent supports **conversational back-and-forth**: it can ask the caller a
clarifying question and wait for the answer. A2A models this natively via its task
lifecycle `input-required` state, with the calling agent replying on the same
`contextId`. MCP (host→tool) has no clean "the server asks the caller a question
and waits" turn. So A2A is primary; REST and an MCP-mount `ask` tool are provided
alongside it ("one capability, three transports").

| Transport | Endpoint | Best for |
|-----------|----------|----------|
| **A2A** (primary) | `/.well-known/agent.json` + A2A task API | agent-to-agent, multi-turn clarification |
| **REST** | `POST /v1/ask`, `GET /healthz` | quick local/manual testing, simple integrations |
| **MCP mount** | streamable-HTTP `ask` tool | MCP-native hosts mounting this agent as a tool |

---

## Prerequisites

1. **`sempkg` binary** on `PATH` (build with
   `cargo build --release --manifest-path src/sempkg/Cargo.toml`).
2. **A synced workspace** — a directory with `sempkg.toml` and installed bundles
   (`sempkg sync` / `sempkg install …`). The agent launches `sempkg mcp -C <workspace>`.
3. **An OpenRouter API key** in `OPENROUTER_API_KEY` (also used by the sempkg
   reranker subprocess).

> ⚠️ Running the agent makes paid LLM + reranker calls through OpenRouter. Start
> with a cheap `SEMPKG_AGENT_MODEL` and the guardrails (`MAX_ITERATIONS`,
> `MAX_FINDINGS`) while you monitor cost.

---

## Install & run (local)

```bash
cd src/sempkg_agent
uv venv && uv pip install -e ".[dev]"
cp .env.example .env   # then edit OPENROUTER_API_KEY, model, workspace

# Primary A2A transport
sempkg-agent serve --transport a2a

# Or a curl-able REST endpoint
sempkg-agent serve --transport rest

# Or every transport at once for testing (A2A=8900, REST=8901, MCP=8902)
sempkg-agent serve --transport all
```

Print the A2A AgentCard (for registration/debugging):

```bash
sempkg-agent card
```

### REST quickstart

```bash
curl -s localhost:8900/v1/ask \
  -H 'content-type: application/json' \
  -d '{"prompt": "How does query expansion route lexical vs vector variants?"}' | jq
```

Scope to a package (the agent may still consult closely related packages):

```bash
curl -s localhost:8900/v1/ask -H 'content-type: application/json' \
  -d '{"prompt": "Where is the BM25 index opened?", "package": "lancedb"}' | jq
```

Multi-turn: if the response `kind` is `clarification`, answer it by re-posting
with the same `session_id` and your clarifying detail in `prompt`.

---

## Inspecting agent behaviour (tracing)

To watch the LLM's reasoning and every sempkg tool call/result, set
`SEMPKG_AGENT_TRACE=1`. Each step is logged to the `sempkg_agent.trace` logger
(stderr), e.g.:

```
sempkg_agent.trace: LLM → tool call: query({"package": "sempkg", "query": "..."})
sempkg_agent.trace: sempkg tool ► query  input={'package': 'sempkg', ...}
sempkg_agent.trace: sempkg tool ◄ query  output=## Query results for: ...
sempkg_agent.trace: LLM reasoning: I can see the key is the `ExpansionKind` enum...
sempkg_agent.trace: LLM → tool call: read_symbol({"package": "sempkg", "symbol": "ExpansionKind"})
```

This is the cheap, self-contained option (logs stay local). For a richer hosted
trace UI, set the standard LangSmith env vars instead
(`LANGCHAIN_TRACING_V2=true` + `LANGCHAIN_API_KEY`) — note that sends traces to
LangSmith's cloud.

## Configuration

All settings are environment variables (see [`.env.example`](.env.example)).
Highlights:

| Variable | Default | Purpose |
|----------|---------|---------|
| `OPENROUTER_API_KEY` | — | LLM + sempkg reranker key |
| `SEMPKG_AGENT_MODEL` | `anthropic/claude-3.5-sonnet` | any OpenRouter model slug |
| `SEMPKG_AGENT_API_BASE` | `https://openrouter.ai/api/v1` | OpenAI-compatible endpoint |
| `SEMPKG_AGENT_MCP_WORKSPACE` | `.` | workspace with `sempkg.toml` + bundles |
| `SEMPKG_AGENT_MAX_ITERATIONS` | `12` | tool-call round ceiling (cost guard) |
| `SEMPKG_AGENT_MAX_FINDINGS` | `12` | cap on returned findings |
| `SEMPKG_AGENT_TRACE` | `0` | log LLM reasoning + every sempkg tool call/result |
| `SEMPKG_AGENT_PORT` | `8900` | bind port |
| `SEMPKG_AGENT_PUBLIC_URL` | `http://localhost:8900` | URL advertised in the AgentCard |
| `SEMPKG_AGENT_AUTH_TOKEN` | — | optional bearer token for REST |

---

## Deploy (Docker)

```bash
docker build -t sempkg-agent:0.1.0 .
docker run --rm -p 8900:8900 \
  -e OPENROUTER_API_KEY=sk-or-... \
  -e SEMPKG_AGENT_MODEL=anthropic/claude-3.5-sonnet \
  -v /path/to/sempkg:/usr/local/bin/sempkg:ro \
  -v /path/to/workspace:/workspace:ro \
  sempkg-agent:0.1.0 serve --transport a2a
```

Or `docker compose up` (see `docker-compose.yml`). For production, run **one
transport per service/container** so each scales independently.

---

## Tests

Tests are fully offline — the LLM and the sempkg MCP server are mocked, so **no
OpenRouter calls are made**.

```bash
uv pip install -e ".[dev]"
pytest
```

---

## Performance & reliability notes

- **`query` latency is the dominant cost.** Each `query` fans out across every
  index and reranks; with a *cloud* reranker (e.g. the OpenRouter reranker in
  `sempkg.toml`) and/or modest hardware a single call can take ~100s. The agent is
  prompted to call `query` at most twice and prefer the cheap `read_*` tools after.
  Tune `[reranker]` in `sempkg.toml` (local reranker / smaller `top_k`) to speed it up.
- **Guaranteed termination.** If the model keeps calling tools without converging
  (more common with smaller/faster models), the agent catches the recursion limit
  and forces one final grounded synthesis from what it already retrieved, so callers
  always get a structured answer instead of an error. A stronger model
  (e.g. `anthropic/claude-3.5-sonnet`) typically converges without the fallback.
- **HTTP timeouts:** because a full run can take minutes on slow retrieval, set
  generous client/proxy timeouts (or use the A2A streaming task updates, which emit
  `working` immediately and don't rely on one long request).

## Production notes / next steps

- **Conversation state** uses an in-process LangGraph `MemorySaver`. For
  horizontal scaling, swap in a persistent checkpointer (Postgres/Redis) — the
  interface is identical.
- **Auth/rate-limiting:** the REST surface supports a bearer token; put A2A/MCP
  behind your gateway for quotas and abuse protection.
- **Observability:** add OpenTelemetry / Langfuse tracing around `agent.ask` to
  measure grounding quality and cost (see the plan doc, §7–§8).
- **Bundle cache / routing:** this prototype queries whatever the configured
  workspace has installed. The hosted vision adds hot/cold bundle tiering and a
  package router (plan doc §4–§6).

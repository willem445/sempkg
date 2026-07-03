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
[`docs/plans/plan-knowledge-agent-server.md`](../../docs/plans/plan-knowledge-agent-server.md).

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
| **REST / chat** | `POST /v1/ask`, `POST /v1/ask/stream`, `GET /` (chat UI) | manual testing, simple integrations, humans |
| **MCP mount** | streamable-HTTP: `ask` + the raw sempkg retrieval tools | MCP-native hosts — either ask the agent, or drive `query`/`read_*` yourself |

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

### Chat UI

A built-in, copilot-style chat UI is served at **`/`** by the `rest`/`chat`
transports (and on the REST port under `all`):

```bash
sempkg-agent serve --transport chat      # UI at http://localhost:8900/
# or
sempkg-agent serve --transport all       # UI at http://localhost:8901/
```

The UI provides:
- a **question → grounded answer** chat: a Markdown **prose answer** a person can
  read, followed by **source cards** (package@version, file + line range, snippet,
  per-source reasoning, and a ✓/⚠ citation-check badge);
- a **"show activity" toggle** that streams the agent's reasoning and every sempkg
  tool call/result live (like Claude Code / Copilot), via SSE;
- a **release box**: leave blank for the latest version, or scope to a release
  (e.g. `v14.2.0`) — see *Versioned retrieval* below;
- a **model dropdown** routed to OpenRouter, populated from a curated catalog
  (`GET /v1/models`). Edit the list in `models.py` or override with
  `SEMPKG_AGENT_MODEL_CATALOG`;
- multi-turn **clarification**: if the agent asks a question, just reply.

### Persona, branding & your own prompt

The agent defaults to a **human** persona — an assistant in front of installed code
+ docs that answers in prose and cites its sources. That's just the default:

- name it with `SEMPKG_AGENT_UI_TITLE` / `SEMPKG_AGENT_UI_SUBTITLE`;
- **replace the behaviour entirely** with your own prompt via
  `SEMPKG_AGENT_SYSTEM_PROMPT` (inline) or `SEMPKG_AGENT_SYSTEM_PROMPT_FILE` (path);
- switch to the machine-to-machine persona with `SEMPKG_AGENT_MODE=agent`.

Branding + installed-knowledge info are exposed at `GET /v1/config`.

### Grounding & verification

Every cited snippet is checked **deterministically** against the evidence the agent
actually retrieved (no extra LLM/tool calls) and flagged `verified` true/false, so a
hallucinated citation is caught rather than trusted. Toggle with
`SEMPKG_AGENT_VERIFY_CITATIONS`. The agent is also prompted to say *"that's not in
the installed bundles"* instead of answering from general knowledge.

### Versioned retrieval

Ask generally (defaults to the **latest** installed version) or scope to a release
via the UI release box / the `version` field on REST + MCP. Note: cleanly isolating
a specific release when several versions are installed depends on a `version` filter
in the sempkg core query layer — see *Versioned retrieval* in
[`deploy/README.md`](../../deploy/README.md). Today the scope is a soft hint.

Scope a question to a package by prefixing `@package`. The streaming endpoint is
`POST /v1/ask/stream` (SSE).

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
| `SEMPKG_AGENT_MODE` | `human` | persona: `human` (prose + cited sources) or `agent` |
| `SEMPKG_AGENT_SYSTEM_PROMPT(_FILE)` | — | replace the built-in prompt with your own |
| `SEMPKG_AGENT_UI_TITLE` / `_SUBTITLE` | generic | chat-UI branding |
| `SEMPKG_AGENT_VERIFY_CITATIONS` | `1` | deterministic citation grounding check |
| `SEMPKG_AGENT_STATE_DB` | — | SQLite path for persistent multi-turn state (`[persist]`) |
| `SEMPKG_AGENT_MAX_ITERATIONS` | `12` | tool-call round ceiling (cost guard) |
| `SEMPKG_AGENT_MAX_FINDINGS` | `12` | cap on returned findings |
| `SEMPKG_AGENT_TRACE` | `0` | log LLM reasoning + every sempkg tool call/result |
| `SEMPKG_AGENT_PORT` | `8900` | bind port |
| `SEMPKG_AGENT_PUBLIC_URL` | `http://localhost:8900` | URL advertised in the AgentCard |
| `SEMPKG_AGENT_AUTH_TOKEN` | — | optional bearer token for REST |

> **Hosted deployment (registry + agent + MCP):** see [`deploy/`](../../deploy/) for
> a docker-compose stack and a GitHub Actions workflow that publishes a rolling
> "latest" bundle (tip of main) plus tagged releases to the registry.

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

The default suite is fully offline — the LLM and the sempkg MCP server are mocked,
so **no OpenRouter calls are made**.

```bash
uv pip install -e ".[dev]"
pytest
```

### Functional (end-to-end) tests

Marked `functional` and **deselected by default**. They self-skip when their
prerequisites are absent, so they're safe to invoke anywhere:

```bash
# Publish a rolling "latest" bundle into a workspace and verify it installs.
# Needs the built sempkg + sembundle binaries; spins up a real registry. No LLM.
pytest -m functional -k publish

# The agent server answering over the real sempkg backend (paid + slow — opt-in).
SEMPKG_AGENT_FUNCTIONAL=1 OPENROUTER_API_KEY=sk-or-... \
  SEMPKG_AGENT_MCP_WORKSPACE=/path/to/workspace \
  pytest -m functional -k backend
```

- `test_publish_corpus_functional.py` — `sembundle build → POST /publish → sempkg
  sync`: the exact flow a CI job uses to push tip-of-main as `latest` into the
  agent's corpus.
- `test_agent_backend_functional.py` — boots the real `KnowledgeAgent` (launches
  `sempkg mcp`, loads the indexes) and drives `/v1/ask` through an in-process ASGI
  client, asserting a grounded, citation-verified answer.

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

# Hosted knowledge-agent deployment

Compose a **registry** + **agent** into one self-hostable service (AWS or on-prem
build server). Teams publish version-pinned bundles of their code + docs to the
registry from CI; the agent installs them and answers questions about them —
through a human chat UI and, for other agents, the standard sempkg MCP tools.

```
   CI (GitHub Actions)                      people                 calling agents
   bundle tip-of-main  ──POST /publish──▶  ┌─────────┐            ┌──────────────┐
   + tagged releases                        │  chat   │            │ MCP client   │
          │                                 │   UI    │            │ (query/read) │
          ▼                                 └────┬────┘            └──────┬───────┘
   ┌──────────────┐   sempkg sync   ┌────────────┴───────── agent ───────┴────────┐
   │   registry   │ ───────────────▶│  REST + UI :8901   ·   sempkg MCP :8902      │
   │   :8765      │   (curated      │  A2A :8900         ·   (raw retrieval tools) │
   │  bundles     │    bundles)     │   grounded answers · cited sources           │
   └──────────────┘                 └──────────────────────────────────────────────┘
```

## Quick start

```bash
cd deploy
export OPENROUTER_API_KEY=sk-or-...
export REGISTRY_ADMIN_PASSWORD=choose-a-strong-secret
export SEMPKG_BIN=/absolute/path/to/sempkg   # or bake it into the agent image
docker compose up --build
```

- Human chat UI → http://localhost:8901/
- Standard sempkg MCP (for agents) → http://localhost:8902/ (streamable-HTTP)
- Registry API → http://localhost:8765/

The agent runs `sempkg sync` on startup to install whatever `workspace/sempkg.toml`
declares from the registry, then serves.

## Publishing bundles from CI

1. Mint a publish token (one time):

   ```bash
   curl -s -X POST http://localhost:8765/admin/tokens \
     -H "Authorization: Bearer $REGISTRY_ADMIN_PASSWORD" \
     -H "content-type: application/json" -d '{"label":"github-actions"}'
   # -> {"token":"<copy this once>", ...}
   ```

2. Add `REGISTRY_URL` and `REGISTRY_TOKEN` as repo secrets and drop
   [`examples/github-actions-publish-latest.yml`](examples/github-actions-publish-latest.yml)
   into `.github/workflows/`. It publishes a rolling **`latest`** bundle on every
   push to `main` and an immutable bundle for every `v*` tag.

3. The agent picks up new bundles on its next sync (restart, or run `sempkg sync`
   in the container on a schedule).

## Versioned retrieval (latest vs a specific release)

This is the one piece that needs support in the **sempkg core query layer**, not
just here. The agent already lets a user ask generally (defaults to *latest*) or
scope to a release (the **release** box in the UI, or `"version"` in the REST/MCP
request). But once you install *multiple versions of the same package* so people
can ask "in v14.2.0, how did X work?", a raw `query` across the workspace will mix
results from every installed version.

To make versioned retrieval clean, the `query`/`search_*` tools should support:

- **default-to-latest** — with no version filter, search only the newest installed
  version of each package (so "how does X work?" isn't diluted by old releases); and
- **a `version` filter** — when the caller passes one, restrict retrieval to that
  version of the bundle.

Until the query tools accept a `version` argument, treat the release scope as a
*soft* hint (the agent is told which version to favour, but the index still spans
all installed versions). Two interim options:

- install only `latest` (single version) for the everyday assistant; or
- give each pinned release a distinct package name (e.g. `ourcode-v14_2_0`) and ask
  with `@ourcode-v14_2_0`, so the existing per-package scoping isolates it.

See the tracking note in `docs/design/agent-server-prototype.md`.

## Exposing MCP for agents

Port **8902** serves the standard sempkg retrieval tools (`query`, `read_code`,
`read_docs`, `read_symbol`, the call-graph tools) over streamable-HTTP, plus a
high-level `ask` tool. A calling agent can mount it and drive retrieval itself
against the same curated, version-pinned bundles — no agent loop in the middle.

## Custom behaviour & branding

- `SEMPKG_AGENT_UI_TITLE` / `SEMPKG_AGENT_UI_SUBTITLE` — name the assistant.
- `SEMPKG_AGENT_SYSTEM_PROMPT` or `SEMPKG_AGENT_SYSTEM_PROMPT_FILE` — replace the
  built-in "assistant in front of code + docs" prompt with your own behaviour.
- `SEMPKG_AGENT_MODE=agent` — switch to the machine-to-machine persona.
- `SEMPKG_AGENT_VERIFY_CITATIONS=0` — disable the deterministic citation check.

## AWS / on-prem notes

- Put the registry's storage volume on durable storage (EBS / a host bind mount);
  bundles and publish tokens live there.
- Terminate TLS at a load balancer / reverse proxy in front of both services.
- Set `SEMPKG_AGENT_AUTH_TOKEN` to require a bearer token on the REST/UI surface,
  and keep the MCP/A2A ports behind your network boundary or gateway.
- The first `query` after a cold start is slow while sempkg loads its models; the
  warm session keeps subsequent calls fast. Prefer a local reranker (see the
  sample `workspace/sempkg.toml`).

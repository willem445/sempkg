# Model Providers

`sempkg` uses three model capabilities internally — **embedding**, **reranking**, and
**query expansion** — each of which can be driven by a different backend.  The backend
is set per-section in `sempkg.toml` via a `provider` field; the default is `"local"`,
which preserves existing behaviour.

## Supported providers

| `provider` value | Backend | Requires |
|---|---|---|
| `"local"` (default) | In-process GGUF via llama-cpp-2 | `reranker` / `embeddings` cargo feature |
| `"openai"` | Any OpenAI-compatible HTTP API | Network access + API key env var |
| `"copilot"` | GitHub Copilot endpoint | _Future phase_ |

Remote providers (`"openai"`) always compile — no native toolchain needed.  Switching
providers requires no code change, only a config edit.

---

## Local GGUF (default)

The default config runs the bundled GGUF model locally.

```toml
[embedding]
enabled = true
provider = "local"          # optional — "local" is the default

[reranker]
enabled = true
provider = "local"

[query_expansion]
enabled = true
provider = "local"
```

### Customising the download URL

Each section accepts a `model_url` field to override the built-in HuggingFace URL used
by `sempkg <section> pull`:

```toml
[embedding]
enabled  = true
model_url = "https://huggingface.co/my-org/my-model/resolve/main/model.gguf"

[reranker]
enabled  = true
model_url = "https://huggingface.co/my-org/reranker/resolve/main/reranker.gguf"

[query_expansion]
enabled  = true
model_url = "https://huggingface.co/my-org/expander/resolve/main/expander.gguf"
```

Priority for the download URL: `--gguf-url` CLI flag > `model_url` in toml > built-in
constant.

---

## OpenAI-compatible remote endpoint (`"openai"`)

Works with OpenAI, OpenRouter, Ollama, LM Studio, vLLM, and any other service that
speaks the OpenAI wire format.

**API keys are read from an environment variable at runtime — they are never stored in
`sempkg.toml`.**  Set `api_key_env` to the name of the variable (default:
`OPENAI_API_KEY`).

### Embeddings via OpenAI

```toml
[embedding]
enabled  = true
provider = "openai"

[embedding.openai]
api_base    = "https://api.openai.com/v1"
model       = "text-embedding-3-small"
api_key_env = "OPENAI_API_KEY"
dim         = 1536            # must match the model's output dimension
```

### Embeddings via OpenRouter

```toml
[embedding]
enabled  = true
provider = "openai"

[embedding.openai]
api_base    = "https://openrouter.ai/api/v1"
model       = "openai/text-embedding-3-small"
api_key_env = "OPENROUTER_API_KEY"
dim         = 1536
```

### Embeddings via Ollama (local, no key required)

```toml
[embedding]
enabled  = true
provider = "openai"

[embedding.openai]
api_base    = "http://localhost:11434/v1"
model       = "nomic-embed-text"
api_key_env = "OLLAMA_API_KEY"   # set to any non-empty value; Ollama ignores it
dim         = 768
```

### Reranker via OpenAI chat completions

Remote reranking uses chat-completion scoring: the model is asked to judge each
(query, document) pair with a yes/no relevance verdict.  `P(yes)` is derived from
`logprobs` when the API returns them, otherwise the model's text reply is parsed.

```toml
[reranker]
enabled   = true
provider  = "openai"
top_k     = 20       # candidates sent to the model for scoring
output_n  = 5        # results to keep after reranking

[reranker.openai]
api_base    = "https://openrouter.ai/api/v1"
model       = "openai/gpt-4o-mini"
api_key_env = "OPENROUTER_API_KEY"
```

### Query expansion via OpenAI

```toml
[query_expansion]
enabled      = true
provider     = "openai"
max_variants = 6

[query_expansion.openai]
api_base    = "https://api.openai.com/v1"
model       = "gpt-4o-mini"
api_key_env = "OPENAI_API_KEY"
```

The model is prompted to return variants in the same `lex:` / `vec:` / `hyde:` format
used by the local GGUF model, so the rest of the pipeline is unchanged.

---

## Mixing providers

Each section is independent.  You can use a local embedder with a remote reranker:

```toml
[embedding]
enabled  = true
provider = "local"      # fast local embeddings

[reranker]
enabled  = true
provider = "openai"     # stronger remote reranker

[reranker.openai]
api_base    = "https://api.openai.com/v1"
model       = "gpt-4o-mini"
api_key_env = "OPENAI_API_KEY"

[query_expansion]
enabled = false         # disabled
```

---

## Changing the embedding provider

The embedding model identity is stamped in the LanceDB index at build time.  If you
switch `[embedding] provider` (or change `model` / `api_base`) you must re-run
`sempkg embed` to rebuild the index with the new model.  Until you do, vector search
falls back to BM25-only — the result set degrades silently rather than erroring.

`sempkg query` prints a note when the active embedder's model ID does not match the
index's recorded model ID.

---

## `pull` commands and remote providers

`sempkg embedding pull`, `sempkg reranker pull`, and `sempkg query-expansion pull`
download and cache the local GGUF file.  They are a no-op (with a clear message) when
`provider` is set to `"openai"` or `"copilot"` — remote providers need no local
model file.  They are also a no-op when the GGUF is already on disk, so re-running a
`pull` (or restoring `~/.sempkg/models/` from a CI cache) costs nothing.

### HuggingFace authentication

All three `pull` commands take the same optional token: `--hf-token`, falling back to
the `HF_TOKEN` environment variable.  The explicit flag wins when both are set — this
is just clap's `env` fallback, not a second auth path.  The default model repos are
public, so the token is only needed for gated repos or to escape anonymous
rate-limiting.

Where the token is sent is deliberately narrow, mirroring what `registry.rs` already
does with GitHub tokens (`github_token_for_url`):

* **Only to `huggingface.co` / `hf.co`, over HTTPS.**  A `--gguf-url` or `model_url`
  can point anywhere, and a credential must not follow it to an arbitrary mirror.
* **Not to the CDN.**  An authenticated `…/resolve/…` request answers with a redirect
  to a *pre-signed* CDN URL (`cdn-lfs.hf.co`, `cas-bridge.xethub.hf.co`).  That URL
  already carries the entitlement of the request that minted it, so the CDN leg needs
  no credential — and a bearer token is an extra auth mechanism a pre-signed URL may
  reject outright.  `reqwest` drops `Authorization` across a host change for exactly
  this reason; the host allowlist states the intent rather than leaning on that.
* **Blank means anonymous.**  An absent GitHub Actions secret expands to an empty
  string, so `HF_TOKEN: ${{ secrets.HF_TOKEN }}` must degrade to an unauthenticated
  download rather than send `Authorization: Bearer ` and get a 401.  (`github.rs`
  applies the same empty-is-unset rule to its tokens.)

The header is marked sensitive, so a `{:?}` of the request, its headers, or a reqwest
error that renders them prints `Sensitive` instead of the token.

---

## GitHub Copilot provider (future)

`provider = "copilot"` is reserved for a future phase.  It will reuse the OpenAI
request format with Copilot's OAuth token exchange and the
`https://api.githubcopilot.com` endpoint.  Setting it today returns a "not yet
implemented" error at startup.

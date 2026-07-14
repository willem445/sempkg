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

### Model downloads are anonymous — by decision

**sempkg sends no credential when it downloads a model, and has no way to be given
one** (#106).  `download_file` builds a plain `GET`; there is no token parameter, no
`Authorization` header, no `HF_TOKEN` environment variable, and no `--hf-token` flag.

The reasoning is a deliberate trade, not an oversight.  The default model repos are
public, so a token buys nothing in the common case — while accepting one means owning
the whole problem of handling a user secret safely: keeping it out of logs, out of
error messages that render URLs and headers, off redirect hops to third-party CDNs,
and out of requests to whatever host `--gguf-url` happens to point at.  That is a
standing liability in exchange for a rare convenience, so the project declines it.
The cost is real and accepted: **sempkg cannot download a gated model**, and cannot
authenticate its way past a HuggingFace rate limit.  The answer to both is manual
placement (below).

> **Contract change.**  `--hf-token` used to exist on all three `pull` commands (and
> accepted `HF_TOKEN` from the environment).  It was **removed**, not deprecated —
> the flag is gone and passing it is now an error.  Scripts that passed a token must
> drop the flag; since the default repos are public, nothing else changes for them.
> `cli::tests::pull_commands_accept_no_token_at_all` pins this so the flag cannot
> drift back in.

Note this is only about *model downloads*.  Remote inference providers
(`provider = "openai"`) still authenticate with their own API key, and `registry.rs`
still uses GitHub tokens for bundle downloads.  Those are separate, opt-in code paths.

### Failed downloads point at manual placement

Since sempkg cannot authenticate its way past an outage or a rate limit, a failed
download is often not retryable in the moment — so the failure has to leave the user
able to finish the job themselves.  Every `pull` path fails with the same notice
(`reranker::manual_placement_notice`): the model URL to fetch, the *exact* path to
save it to, and the fact that the next run picks the file up instead of re-downloading
it.  That escape hatch only works if we say precisely where the file goes, so the
message spells it out rather than making the user derive it from config.

---

## GitHub Copilot provider (future)

`provider = "copilot"` is reserved for a future phase.  It will reuse the OpenAI
request format with Copilot's OAuth token exchange and the
`https://api.githubcopilot.com` endpoint.  Setting it today returns a "not yet
implemented" error at startup.

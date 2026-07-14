/// Optional Qwen3-Reranker-0.6B GGUF reranker for CodeGraph + LanceDB results.
///
/// When built with `--features reranker` and a model is configured/present,
/// queries are run through a local Qwen3-based cross-encoder that scores
/// (query, document) pairs and re-sorts results by relevance.
///
/// **Backend**: llama-cpp-2 (utilityai/llama-cpp-rs), which wraps llama.cpp and
/// has native Qwen3 + RANK-pooling reranker support. The tokenizer is embedded
/// inside the GGUF file — no separate tokenizer.json is required.
///
/// **Inference pattern (pointwise reranking)**:
/// 1. Build the Qwen3-Reranker chat prompt for each (query, doc) pair.
/// 2. Run llama_encode() — a single encoder-style forward pass.
/// 3. Read the RANK-pooled scalar score from llama_get_embeddings_seq().
/// 4. Apply sigmoid to convert the raw logit to [0, 1].
/// 5. Re-sort candidates descending by score.
///
/// When built without `--features reranker` the module compiles to no-ops so
/// the rest of the codebase doesn't need `#[cfg]` guards.
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::providers::{OpenAiProviderConfig, ProviderKind, Rerank};

// ---------------------------------------------------------------------------
// Configuration  (always present — read from [reranker] in sempkg.toml)
// ---------------------------------------------------------------------------

/// Mirrors the `[reranker]` table in `sempkg.toml`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RerankerConfig {
    /// Whether reranking is active.  Defaults to `true` when the section exists.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Which backend to use. `"local"` (default) requires the `reranker` cargo
    /// feature; `"openai"` uses chat-completion scoring via any OpenAI-compatible
    /// HTTP endpoint.
    #[serde(default)]
    pub provider: ProviderKind,

    /// Path to the GGUF model file.
    /// May use `~` for the home directory.
    /// Only used when `provider = "local"`.
    pub model: Option<String>,

    /// HuggingFace (or other) URL to download the GGUF from.
    /// Overrides the built-in default URL when running `sempkg reranker pull`.
    /// Only used when `provider = "local"`.
    pub model_url: Option<String>,

    /// OpenAI-compatible provider settings. Required when `provider = "openai"`.
    pub openai: Option<OpenAiProviderConfig>,

    /// Number of BM25 candidates passed into the model for scoring.
    /// Defaults to 20.
    #[serde(default = "default_top_k")]
    pub top_k: usize,

    /// Number of results returned after reranking.
    /// Defaults to 5.
    #[serde(default = "default_output_n")]
    pub output_n: usize,

    /// GPU offload policy: `"auto"` (default), `"on"`, or `"off"`. Only takes
    /// effect on a GPU-backend build (`--features reranker,cuda` or `…,vulkan`);
    /// see [`crate::accel::GpuMode`]. Only used when `provider = "local"`.
    #[serde(default)]
    pub gpu: crate::accel::GpuMode,

    /// CPU threads for reranker inference. `0` (default) uses all logical cores.
    /// Only used when `provider = "local"`.
    #[serde(default)]
    pub n_threads: u32,

    /// Advanced override: offload exactly this many model layers to the GPU.
    /// `0` (default) defers to `gpu` (auto-detect). Requires a GPU-backend build.
    /// Only used when `provider = "local"`.
    #[serde(default)]
    pub gpu_layers: u32,
}

fn default_true() -> bool {
    true
}
fn default_top_k() -> usize {
    20
}
fn default_output_n() -> usize {
    5
}

// A derived `Default` would zero `top_k`/`output_n` (the `#[serde(default)]`
// attributes only apply during deserialization), which makes the reranker
// silently discard every candidate via `.take(0)`. Implement it by hand so the
// no-manifest defaults match the serde defaults.
impl Default for RerankerConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            provider: ProviderKind::Local,
            model: None,
            model_url: None,
            openai: None,
            top_k: default_top_k(),
            output_n: default_output_n(),
            gpu: crate::accel::GpuMode::default(),
            n_threads: 0,
            gpu_layers: 0,
        }
    }
}

impl RerankerConfig {
    /// Resolve the model path, expanding `~`.
    pub fn resolved_model_path(&self) -> PathBuf {
        let raw = self
            .model
            .clone()
            .unwrap_or_else(|| default_model_path().to_string_lossy().to_string());
        expand_tilde(&raw)
    }
}

/// Default model download directory.
pub fn default_model_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".sempkg")
        .join("models")
}

/// Full path to the default GGUF file.
pub fn default_model_path() -> PathBuf {
    default_model_dir().join("qwen3-reranker-0.6b-q8_0.gguf")
}

// ---------------------------------------------------------------------------
// Public candidate / result types (always present)
// ---------------------------------------------------------------------------

/// A single item that can be passed into the reranker for scoring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RerankOrigin {
    Codegraph,
    Docs,
}

/// A single item that can be passed into the reranker for scoring.
#[derive(Debug, Clone)]
pub struct RerankCandidate {
    /// Human-readable identifier (file path, symbol name, …).
    pub source: String,
    /// Text content used for scoring.
    pub text: String,
    /// Retrieval source this candidate came from.
    pub origin: RerankOrigin,
}

/// A scored result after reranking.
#[derive(Debug, Clone)]
pub struct RerankResult {
    pub source: String,
    pub text: String,
    pub origin: RerankOrigin,
    /// Relevance score in [0, 1].  Higher is more relevant.
    pub score: f32,
}

// ---------------------------------------------------------------------------
// Model management helpers (always compiled)
// ---------------------------------------------------------------------------

/// Expand a leading `~` to the home directory.
pub fn expand_tilde(path: &str) -> PathBuf {
    if let Some(stripped) = path.strip_prefix("~/") {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(stripped)
    } else if path == "~" {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
    } else {
        PathBuf::from(path)
    }
}

/// Returns `true` when the GGUF model file exists.
/// (The tokenizer is embedded in the GGUF — no separate tokenizer.json needed.)
pub fn model_is_present(config: &RerankerConfig) -> bool {
    config.resolved_model_path().is_file()
}

// ---------------------------------------------------------------------------
// Default HuggingFace download URLs
// ---------------------------------------------------------------------------

/// GGUF download URL — ggml-org's Q8_0 quant of Qwen3-Reranker-0.6B.
/// This repo is public (Apache-2.0) and does not require authentication.
pub const DEFAULT_GGUF_URL: &str =
    "https://huggingface.co/ggml-org/Qwen3-Reranker-0.6B-Q8_0-GGUF/resolve/main/qwen3-reranker-0.6b-q8_0.gguf";

/// Build the `GET` for a model download.
///
/// It carries **no credential**, and there is no parameter through which one could
/// be supplied — see the module docs and `docs/design/model-providers.md` (#106).
/// This exists as a named seam so that fact is directly testable, the same way
/// [`crate::registry::direct_download_request`] makes GitHub's auth testable.
fn model_download_request(
    client: &reqwest::blocking::Client,
    url: &str,
) -> reqwest::blocking::RequestBuilder {
    client.get(url)
}

/// What to tell a user whose download came back `401 Unauthorized`.
///
/// sempkg sends no credentials, so a 401 is not something the user can fix by
/// supplying one — the URL simply isn't reachable anonymously. Say that, and point
/// at the two things that do work.
fn unauthorized_notice(url: &str, dest: &Path) -> String {
    format!(
        "HTTP 401 Unauthorized for {url}\n\n\
         This URL requires authentication, but sempkg downloads models anonymously \
         and sends no credentials.\n\n\
         Point `--gguf-url` at a public GGUF, or download the file yourself and save \
         it to:\n\
         \n    {dest}\n\n\
         sempkg picks it up on the next run.",
        dest = dest.display()
    )
}

/// Base delay between download retries; the nth retry waits `n × RETRY_BACKOFF`.
///
/// Zero under `cfg(test)`. The offline failure-path tests point `download_file` at a
/// refused port, so every attempt fails instantly and the only thing the backoff
/// buys is seconds of sleeping in the test suite. The retry *count* is unchanged, so
/// the tests still exercise the real retry loop.
#[cfg(not(test))]
const RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_secs(2);
#[cfg(test)]
const RETRY_BACKOFF: std::time::Duration = std::time::Duration::ZERO;

/// Download a file with a simple progress indicator and save it to `dest`.
///
/// The request is **anonymous** and sempkg carries no credential for it: the model
/// repos are public, and the project deliberately does not take on the risk of
/// handling a user's HuggingFace token (#106).
pub fn download_file(url: &str, dest: &Path) -> Result<()> {
    use std::io::Write;
    use std::time::Duration;

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating directory {}", parent.display()))?;
    }

    // Large GGUF downloads are prone to transient read timeouts; use generous
    // request timeout and a few retries before failing.
    let timeout_secs = std::env::var("SEMPKG_DOWNLOAD_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(1800);
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(timeout_secs))
        .build()
        .context("building HTTP client")?;

    let max_attempts = 3u32;
    for attempt in 1..=max_attempts {
        let req = model_download_request(&client, url);

        let resp = req
            .send()
            .with_context(|| format!("GET {url} (attempt {attempt}/{max_attempts})"));

        let resp = match resp {
            Ok(r) => r,
            Err(e) if attempt < max_attempts => {
                println!("  download attempt {attempt}/{max_attempts} failed: {e}; retrying...");
                std::thread::sleep(RETRY_BACKOFF * attempt);
                continue;
            }
            Err(e) => {
                return Err(e);
            }
        };

        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            anyhow::bail!(unauthorized_notice(url, dest));
        }

        let mut resp = match resp.error_for_status() {
            Ok(r) => r,
            Err(e) if attempt < max_attempts => {
                println!("  download attempt {attempt}/{max_attempts} failed: {e}; retrying...");
                std::thread::sleep(RETRY_BACKOFF * attempt);
                continue;
            }
            Err(e) => return Err(e).with_context(|| format!("HTTP error for {url}")),
        };

        let total = resp.content_length();
        let tmp_dest = dest.with_extension("part");
        let mut file = std::fs::File::create(&tmp_dest)
            .with_context(|| format!("creating {}", tmp_dest.display()))?;

        let copied =
            std::io::copy(&mut resp, &mut file).with_context(|| format!("reading body of {url}"));

        match copied {
            Ok(n) => {
                file.flush()
                    .with_context(|| format!("flushing {}", tmp_dest.display()))?;
                // Close the handle before any rename/remove — Windows refuses both
                // while the file is still open.
                drop(file);

                // A short body must never be renamed into place. hyper already errors
                // on a response that undershoots its Content-Length, so this only bites
                // on a chunked/length-less response that ends cleanly early — but the
                // result would be a truncated GGUF sitting at the model path, which CI
                // then caches under a key claiming it is the real thing. Cheap to rule
                // out; expensive to debug.
                if let Some(expected) = total {
                    if n != expected {
                        let _ = std::fs::remove_file(&tmp_dest);
                        anyhow::bail!(
                            "truncated download for {url}: expected {expected} bytes, got {n}"
                        );
                    }
                }

                std::fs::rename(&tmp_dest, dest).with_context(|| {
                    format!("moving {} to {}", tmp_dest.display(), dest.display())
                })?;

                let bytes_to_report = total.unwrap_or(n);
                println!(
                    "  downloaded {:.1} MiB",
                    bytes_to_report as f64 / 1_048_576.0
                );
                return Ok(());
            }
            Err(e) if attempt < max_attempts => {
                let _ = std::fs::remove_file(&tmp_dest);
                println!("  download attempt {attempt}/{max_attempts} failed: {e}; retrying...");
                std::thread::sleep(RETRY_BACKOFF * attempt);
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp_dest);
                return Err(e);
            }
        }
    }

    anyhow::bail!("download failed for {url}")
}

/// What to tell a user whose model download just failed.
///
/// sempkg downloads anonymously, so an outage or a rate-limit on HuggingFace's
/// side is not something a retry can fix and not something sempkg can
/// authenticate its way around. The fallback that always works is to fetch the
/// file by any other means and drop it where sempkg looks — which only helps if
/// we say *exactly* where that is, so this spells out both the URL and the
/// destination path rather than making the user go find them.
///
/// Shared by every `pull` path (reranker, embedding, query expansion): the advice
/// is identical and only the URL and destination differ.
pub fn manual_placement_notice(url: &str, dest: &Path) -> String {
    format!(
        "Could not download the model from {url}\n\n\
         sempkg downloads models anonymously, so this is usually HuggingFace being \
         unavailable or rate-limiting rather than anything wrong with your setup.\n\n\
         You can place the file by hand instead — download it from:\n\
         \n    {url}\n\n\
         and save it to exactly this path:\n\
         \n    {dest}\n\n\
         sempkg picks it up on the next run: `pull` sees the file and skips the \
         download entirely.",
        dest = dest.display()
    )
}

/// Pull the GGUF model into `~/.sempkg/models/` (or the directory implied
/// by `config`). The tokenizer is embedded inside the GGUF — no separate
/// tokenizer.json download is needed when using llama-cpp-2.
///
/// The download is anonymous; sempkg sends no HuggingFace credential (#106).
pub fn pull_model(config: &RerankerConfig, gguf_url: Option<&str>) -> Result<()> {
    let model_path = config.resolved_model_path();
    // Priority: CLI --gguf-url flag > toml model_url > built-in default
    let source_url = gguf_url
        .or_else(|| config.model_url.as_deref())
        .unwrap_or(DEFAULT_GGUF_URL);

    if model_path.is_file() {
        println!("Model already present: {}", model_path.display());
    } else {
        println!("Downloading {}  →  {}", source_url, model_path.display());
        download_file(source_url, &model_path)
            .with_context(|| manual_placement_notice(source_url, &model_path))?;
        println!("  ✓ model saved.");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// System prompt for Qwen3-Reranker
// ---------------------------------------------------------------------------

const RERANKER_SYSTEM_PROMPT: &str =
    "Judge whether the Document meets the requirements of the Query. \
     Note that the answer can only be \"yes\" or \"no\".";

/// Default task instruction prepended to every (query, document) pair.
/// Mirrors the instruction style Qwen3-Reranker was trained with.
const RERANKER_INSTRUCTION: &str =
    "Given a search query, retrieve relevant code and documentation that answers the query";

/// Build the Qwen3-Reranker chat prompt for a (query, document) pair.
///
/// This must match the exact template the model was trained on (system prompt,
/// `<Instruct>:` / `<Query>:` / `<Document>:` structure, and the empty
/// `<think>` block) — otherwise the RANK-pooled classifier score is unreliable.
fn build_rerank_prompt(query: &str, document: &str) -> String {
    format!(
        "<|im_start|>system\n{RERANKER_SYSTEM_PROMPT}<|im_end|>\n\
         <|im_start|>user\n\
         <Instruct>: {RERANKER_INSTRUCTION}\n\
         <Query>: {query}\n\
         <Document>: {document}<|im_end|>\n\
         <|im_start|>assistant\n<think>\n\n</think>\n\n"
    )
}

// ---------------------------------------------------------------------------
// Reranker struct — full implementation behind the `reranker` feature flag
// ---------------------------------------------------------------------------
//
// Uses llama-cpp-2 (utilityai/llama-cpp-rs) to load the GGUF via llama.cpp.
// The Qwen3-Reranker GGUF sets pooling_type = RANK in its metadata, so
// llama.cpp scores (query, document) pairs and returns a single float logit
// via llama_get_embeddings_seq().  Sigmoid maps it to [0, 1].

/// Loaded reranker ready to score (query, document) pairs.
#[cfg(feature = "reranker")]
pub struct Reranker {
    model: llama_cpp_2::model::LlamaModel,
    /// CPU threads used for scoring contexts (resolved: 0 → all cores).
    n_threads: i32,
    top_k: usize,
    output_n: usize,
}

#[cfg(feature = "reranker")]
impl Reranker {
    /// Load the GGUF model from disk using llama.cpp.
    pub fn load(config: &RerankerConfig) -> Result<Self> {
        use llama_cpp_2::model::{params::LlamaModelParams, LlamaModel};
        use llama_cpp_2::{send_logs_to_tracing, LogOptions};

        // Silence llama.cpp's extremely verbose stderr logging. Routing logs to
        // `tracing` with logging disabled drops them entirely (sempkg installs
        // no tracing subscriber). Must run before any other llama.cpp call.
        static LOG_INIT: std::sync::Once = std::sync::Once::new();
        LOG_INIT.call_once(|| {
            send_logs_to_tracing(LogOptions::default().with_logs_enabled(false));
        });

        let model_path = config.resolved_model_path();
        if !model_path.is_file() {
            anyhow::bail!(
                "Reranker model not found at {}. Run `sempkg reranker pull` to download it.",
                model_path.display()
            );
        }

        // Use the process-wide shared backend (created once, never dropped) so
        // multiple llama models can coexist without a double-free at shutdown.
        let backend = crate::llama_runtime::shared()?;

        let n_gpu_layers =
            crate::accel::resolve_gpu_layers(config.gpu, config.gpu_layers, backend, "reranker");
        let model_params = LlamaModelParams::default().with_n_gpu_layers(n_gpu_layers);
        let model = LlamaModel::load_from_file(backend, &model_path, &model_params)
            .map_err(|e| anyhow::anyhow!("loading model from {}: {e}", model_path.display()))?;

        Ok(Self {
            model,
            n_threads: crate::accel::resolve_threads(config.n_threads),
            top_k: config.top_k,
            output_n: config.output_n,
        })
    }

    /// Score a single (query, document) pair via RANK-pooled encoding.
    ///
    /// A fresh context is created per pair so there is no residual KV-cache
    /// state between calls.  Context creation from a loaded model is cheap.
    /// Score a single `(query, document)` pair, returning P(relevant) in [0, 1].
    ///
    /// Made public so callers that need to bypass the `top_k` / `output_n` caps
    /// inside `rerank()` (e.g. KWIC window scoring in pass-2) can iterate over
    /// windows themselves and call this primitive directly.
    pub fn score_pair(&self, query: &str, document: &str) -> Result<f32> {
        use llama_cpp_2::{
            context::params::LlamaContextParams, llama_batch::LlamaBatch, model::AddBos,
        };

        let prompt = build_rerank_prompt(query, document);

        // Pooling type comes from the GGUF metadata (RANK for Qwen3-Reranker).
        let ctx_params = LlamaContextParams::default()
            .with_n_ctx(std::num::NonZeroU32::new(4096))
            .with_embeddings(true)
            .with_n_batch(4096)
            .with_n_ubatch(4096)
            .with_n_threads(self.n_threads)
            .with_n_threads_batch(self.n_threads);

        let mut ctx = self
            .model
            .new_context(crate::llama_runtime::shared()?, ctx_params)
            .map_err(|e| anyhow::anyhow!("creating context: {e}"))?;

        let mut tokens = self
            .model
            .str_to_token(&prompt, AddBos::Always)
            .map_err(|e| anyhow::anyhow!("tokenizing prompt: {e}"))?;

        // Silently truncate to context window if the prompt is very long.
        let n_ctx = ctx.n_ctx() as usize;
        tokens.truncate(n_ctx);

        let mut batch = LlamaBatch::new(tokens.len(), 1);
        batch
            .add_sequence(&tokens, 0, false)
            .map_err(|e| anyhow::anyhow!("building batch: {e}"))?;

        // Qwen3-Reranker is a causal decoder model, so the forward pass is a
        // `decode` (encode is for encoder-decoder models). RANK pooling then
        // produces one score per sequence.
        ctx.decode(&mut batch)
            .map_err(|e| anyhow::anyhow!("decoding batch: {e}"))?;

        // RANK pooling on the 2-class (yes/no) classifier head returns softmax
        // probabilities. `emb[0]` is P(yes) — the relevance probability — and
        // `emb[1]` is P(no); they sum to 1. Remaining slots in the slice are
        // unused buffer space. Use P(yes) directly as the relevance score.
        let emb = ctx
            .embeddings_seq_ith(0)
            .map_err(|e| anyhow::anyhow!("reading score: {e}"))?;

        Ok(emb.first().copied().unwrap_or(0.0))
    }

    /// Score many documents against one `query`, reusing a single
    /// `LlamaContext` for the whole batch and clearing the KV cache between
    /// decodes — the same pattern as [`crate::embedding::Embedder::
    /// embed_documents_batch`]. This avoids the per-pair context/KV-cache
    /// allocation that dominates `score_pair` when the `tool_query` pipeline
    /// scores an entire candidate pool (pass 1) or every KWIC window (pass 2).
    ///
    /// Returns one score in `[0, 1]` per input document, in order.
    pub fn score_pairs(&self, query: &str, documents: &[&str]) -> Result<Vec<f32>> {
        use llama_cpp_2::{
            context::params::LlamaContextParams, llama_batch::LlamaBatch, model::AddBos,
        };

        if documents.is_empty() {
            return Ok(Vec::new());
        }

        // One context reused for every pair (model + KV cache allocated once).
        // Identical params to `score_pair` so scores are unchanged.
        let ctx_params = LlamaContextParams::default()
            .with_n_ctx(std::num::NonZeroU32::new(4096))
            .with_embeddings(true)
            .with_n_batch(4096)
            .with_n_ubatch(4096)
            .with_n_threads(self.n_threads)
            .with_n_threads_batch(self.n_threads);

        let mut ctx = self
            .model
            .new_context(crate::llama_runtime::shared()?, ctx_params)
            .map_err(|e| anyhow::anyhow!("creating context: {e}"))?;
        let n_ctx = ctx.n_ctx() as usize;

        let mut scores = Vec::with_capacity(documents.len());
        for document in documents {
            let prompt = build_rerank_prompt(query, document);
            let mut tokens = self
                .model
                .str_to_token(&prompt, AddBos::Always)
                .map_err(|e| anyhow::anyhow!("tokenizing prompt: {e}"))?;
            tokens.truncate(n_ctx);

            let mut batch = LlamaBatch::new(tokens.len(), 1);
            batch
                .add_sequence(&tokens, 0, false)
                .map_err(|e| anyhow::anyhow!("building batch: {e}"))?;

            // Reset KV cache before each decode so sequences don't accumulate.
            ctx.clear_kv_cache();
            ctx.decode(&mut batch)
                .map_err(|e| anyhow::anyhow!("decoding batch: {e}"))?;

            let emb = ctx
                .embeddings_seq_ith(0)
                .map_err(|e| anyhow::anyhow!("reading score: {e}"))?;
            scores.push(emb.first().copied().unwrap_or(0.0));
        }

        Ok(scores)
    }
}

#[cfg(feature = "reranker")]
impl Rerank for Reranker {
    fn score_pair(&self, query: &str, doc: &str) -> Result<f32> {
        Reranker::score_pair(self, query, doc)
    }

    fn score_pairs(&self, query: &str, docs: &[&str]) -> Result<Vec<f32>> {
        Reranker::score_pairs(self, query, docs)
    }

    fn top_k(&self) -> usize {
        self.top_k
    }

    fn output_n(&self) -> usize {
        self.output_n
    }
}

// ---------------------------------------------------------------------------
// No-op stub when the feature is disabled
// ---------------------------------------------------------------------------

/// When compiled *without* `--features reranker` this is a zero-sized stub.
#[cfg(not(feature = "reranker"))]
pub struct Reranker;

#[cfg(not(feature = "reranker"))]
impl Reranker {
    /// Always returns an error explaining the feature is not compiled in.
    pub fn load(_config: &RerankerConfig) -> Result<Self> {
        anyhow::bail!(
            "Reranker support is not compiled into this binary. \
             Rebuild with `cargo build --features reranker`."
        )
    }

    pub fn rerank(
        &self,
        _query: &str,
        _candidates: Vec<RerankCandidate>,
    ) -> Result<Vec<RerankResult>> {
        Ok(Vec::new())
    }

    /// Stub — always returns 0.0; real scoring requires `--features reranker`.
    pub fn score_pair(&self, _query: &str, _document: &str) -> Result<f32> {
        Ok(0.0)
    }
}

#[cfg(not(feature = "reranker"))]
impl Rerank for Reranker {
    fn score_pair(&self, query: &str, doc: &str) -> Result<f32> {
        Reranker::score_pair(self, query, doc)
    }
    fn top_k(&self) -> usize {
        20
    }
    fn output_n(&self) -> usize {
        5
    }
}

// ---------------------------------------------------------------------------
// MCP / CLI helpers shared by both configurations
// ---------------------------------------------------------------------------

/// Format reranked results as Markdown, annotating each with its score.
pub fn format_reranked_docs(results: &[RerankResult], query: &str) -> String {
    if results.is_empty() {
        return format!("No results for '{query}'.");
    }
    results
        .iter()
        .map(|r| {
            format!(
                "**{}** _(relevance: {:.2})_\n\n{}",
                r.source, r.score, r.text
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n---\n\n")
}

/// Convert a raw codegraph JSON string into rerank candidates.
///
/// `codegraph query --json` returns an array of search hits, each shaped as
/// `{ "node": { "name", "qualifiedName", "kind", "signature", "filePath", … },
///    "score": <f64> }` — the symbol fields live under `node`. For robustness
/// this also accepts a bare node object (no `node`/`score` envelope).
pub fn codegraph_json_to_candidates(json: &str) -> Vec<RerankCandidate> {
    let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(json) else {
        return Vec::new();
    };
    arr.into_iter()
        .filter_map(|v| {
            // Unwrap the `{ "node": {…}, "score": N }` envelope if present.
            let node = v.get("node").unwrap_or(&v);
            let obj = node.as_object()?;
            let get = |k: &str| obj.get(k).and_then(|x| x.as_str()).unwrap_or("");

            // Prefer the fully-qualified name so identically-named symbols
            // (e.g. several `connect` methods) stay distinguishable.
            let qualified = get("qualifiedName");
            let name = get("name");
            let label = if !qualified.is_empty() {
                qualified
            } else {
                name
            };
            let source = if label.is_empty() {
                "unknown".to_string()
            } else {
                label.to_string()
            };

            // Build a compact natural-language description for the cross-encoder.
            let kind = get("kind");
            let signature = get("signature");
            let file = get("filePath");

            let mut text = String::new();
            if !kind.is_empty() {
                text.push_str(kind);
                text.push(' ');
            }
            text.push_str(label);
            if !signature.is_empty() {
                text.push_str(signature);
            }
            if !file.is_empty() {
                text.push_str(" in ");
                text.push_str(file);
            }

            Some(RerankCandidate {
                source,
                text,
                origin: RerankOrigin::Codegraph,
            })
        })
        .collect()
}

/// Convert `lance::SearchResult` items into rerank candidates.
pub fn lance_results_to_candidates(results: &[crate::lance::SearchResult]) -> Vec<RerankCandidate> {
    results
        .iter()
        .map(|r| RerankCandidate {
            source: if r.start_line > 0 {
                format!("{}:{}-{}", r.path, r.start_line, r.end_line)
            } else {
                r.path.clone()
            },
            text: r.snippet.clone(),
            origin: RerankOrigin::Docs,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Status helper
// ---------------------------------------------------------------------------

pub fn print_status(config: &RerankerConfig) {
    let model_path = config.resolved_model_path();

    println!("Reranker configuration:");
    println!("  enabled    : {}", config.enabled);
    println!("  model      : {}", model_path.display());
    println!("  top_k      : {}", config.top_k);
    println!("  output_n   : {}", config.output_n);
    println!(
        "  cpu threads: {} ({})",
        crate::accel::resolve_threads(config.n_threads),
        if config.n_threads == 0 {
            "all cores"
        } else {
            "configured"
        }
    );
    println!(
        "  gpu        : {}{}",
        config.gpu.as_str(),
        if config.gpu_layers > 0 {
            format!(" (manual override: {} layers)", config.gpu_layers)
        } else {
            String::new()
        }
    );
    println!("  gpu build  : {}", crate::accel::gpu_build_status());
    println!();

    let model_ok = model_path.is_file();
    println!(
        "  model file : {}",
        if model_ok {
            "✓ present"
        } else {
            "✗ missing"
        }
    );
    println!("  tokenizer  : embedded in GGUF (no separate file needed)");

    if model_ok {
        println!();
        println!("Reranker is ready. Queries will be reranked when sempkg mcp is running.");
    } else {
        println!();
        println!("Run `sempkg reranker pull` to download the model.");
    }

    #[cfg(not(feature = "reranker"))]
    {
        println!();
        println!(
            "NOTE: This binary was compiled WITHOUT the `reranker` feature. \
             Reranking is disabled at runtime even if the model is present. \
             Rebuild with `cargo build --features reranker` to enable it."
        );
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A URL no request may ever reach. Port 1 on loopback refuses instantly, so
    /// a `pull_model` that wrongly downloads fails loudly instead of quietly
    /// reaching the network.
    const UNREACHABLE_URL: &str = "http://127.0.0.1:1/never-fetched.gguf";

    fn config_for(model_path: &Path) -> RerankerConfig {
        RerankerConfig {
            model: Some(model_path.to_string_lossy().to_string()),
            ..Default::default()
        }
    }

    /// Both user-facing notices are multi-line string literals held together by
    /// `\n\` continuations. Drop one and the source indentation leaks into the
    /// message as long runs of spaces mid-sentence — exactly what shipped in the 401
    /// message and got caught in review.
    ///
    /// The notices use one deliberate layout: a 4-space indent to offset a URL or a
    /// path onto its own line. Every other line is prose and must carry no
    /// indentation and no interior run of spaces.
    #[test]
    fn user_facing_notices_are_not_mangled_by_source_indentation() {
        let dest = Path::new("/home/u/.sempkg/models/qwen3-reranker-0.6b-q8_0.gguf");

        for notice in [
            unauthorized_notice(DEFAULT_GGUF_URL, dest),
            manual_placement_notice(DEFAULT_GGUF_URL, dest),
        ] {
            for line in notice.lines() {
                // A deliberately offset value: exactly four spaces, then the value.
                let body = match line.strip_prefix("    ") {
                    Some(value) if !value.starts_with(' ') => value,
                    Some(_) => panic!("over-indented line: {line:?}\n\nin:\n{notice}"),
                    None => {
                        assert!(
                            !line.starts_with(' '),
                            "prose line carries leaked source indentation: {line:?}\
                             \n\nin:\n{notice}"
                        );
                        line
                    }
                };

                assert!(
                    !body.contains("  "),
                    "line has a run of spaces mid-sentence: {line:?}\n\nin:\n{notice}"
                );
            }
        }
    }

    /// A 401 must not tell the user to go find a token — sempkg has no way to send
    /// one. It should point at the two things that actually work.
    #[test]
    fn unauthorized_notice_offers_no_token() {
        let dest = Path::new("/models/m.gguf");
        let notice = unauthorized_notice(DEFAULT_GGUF_URL, dest).to_lowercase();

        assert!(
            !notice.contains("token"),
            "401 notice must not offer a token"
        );
        assert!(!notice.contains("hf_token"));
        assert!(notice.contains("anonymously"));
        assert!(notice.contains("--gguf-url"));
        assert!(notice.contains("/models/m.gguf"));
    }

    /// The download is anonymous: sempkg holds no HuggingFace credential, so the
    /// request it puts on the wire must carry no `Authorization` header — for the
    /// default model repo and for any `--gguf-url` a user points it at (#106).
    /// `build()` sends nothing, so this touches no network.
    #[test]
    fn model_downloads_are_anonymous() {
        let client = reqwest::blocking::Client::new();

        for url in [
            DEFAULT_GGUF_URL,
            "https://huggingface.co/some/gated-repo/resolve/main/m.gguf",
            "https://mirror.example.com/m.gguf",
        ] {
            let request = model_download_request(&client, url)
                .build()
                .expect("request should build");

            assert!(
                request
                    .headers()
                    .get(reqwest::header::AUTHORIZATION)
                    .is_none(),
                "model download to {url} must not be authenticated"
            );
        }
    }

    /// `sempkg reranker pull` must no-op when the GGUF is already on disk — that
    /// is what makes a CI model cache hit skip the download entirely. Point it at
    /// a URL that would fail if it were ever fetched.
    #[test]
    fn pull_model_skips_download_when_model_is_present() {
        let dir = tempfile::tempdir().expect("tempdir");
        let model_path = dir.path().join("qwen3-reranker-0.6b-q8_0.gguf");
        std::fs::write(&model_path, b"cached gguf bytes").expect("write model");

        pull_model(&config_for(&model_path), Some(UNREACHABLE_URL))
            .expect("pull must no-op, not download");

        assert_eq!(
            std::fs::read(&model_path).expect("read model"),
            b"cached gguf bytes",
            "the cached model was overwritten by a re-download"
        );
    }

    /// A failed pull must leave the user able to finish the job by hand: it has to
    /// name the URL to fetch, the exact path to save it to, and say that the next
    /// run will pick it up. No network — the URL refuses on connect.
    #[test]
    fn failed_pull_says_where_to_put_the_model_by_hand() {
        let dir = tempfile::tempdir().expect("tempdir");
        let model_path = dir.path().join("qwen3-reranker-0.6b-q8_0.gguf");

        let err = pull_model(&config_for(&model_path), Some(UNREACHABLE_URL))
            .expect_err("an unreachable URL must fail the pull");
        let rendered = format!("{err:?}");

        assert!(
            rendered.contains(UNREACHABLE_URL),
            "failure must name the model URL to fetch:
{rendered}"
        );
        assert!(
            rendered.contains(&model_path.display().to_string()),
            "failure must name the exact placement path:
{rendered}"
        );
        assert!(
            rendered.contains("next run"),
            "failure must say the file is picked up on the next run:
{rendered}"
        );
    }
}

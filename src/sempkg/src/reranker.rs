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

// ---------------------------------------------------------------------------
// Configuration  (always present — read from [reranker] in sempkg.toml)
// ---------------------------------------------------------------------------

/// Mirrors the `[reranker]` table in `sempkg.toml`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RerankerConfig {
    /// Whether reranking is active.  Defaults to `true` when the section exists.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Path to the GGUF model file.
    /// May use `~` for the home directory.
    /// Defaults to `~/.sempkg/models/Qwen3-Reranker-1.7B-Q4_K_M.gguf`.
    pub model: Option<String>,

    /// Number of BM25 candidates passed into the model for scoring.
    /// Defaults to 20.
    #[serde(default = "default_top_k")]
    pub top_k: usize,

    /// Number of results returned after reranking.
    /// Defaults to 5.
    #[serde(default = "default_output_n")]
    pub output_n: usize,
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
            model: None,
            top_k: default_top_k(),
            output_n: default_output_n(),
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

/// Download a file with a simple progress indicator and save it to `dest`.
/// Pass an optional HuggingFace bearer token for gated repositories.
pub fn download_file(url: &str, dest: &Path, hf_token: Option<&str>) -> Result<()> {
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
        let mut req = client.get(url);
        if let Some(tok) = hf_token {
            req = req.bearer_auth(tok);
        }

        let resp = req
            .send()
            .with_context(|| format!("GET {url} (attempt {attempt}/{max_attempts})"));

        let resp = match resp {
            Ok(r) => r,
            Err(e) if attempt < max_attempts => {
                println!("  download attempt {attempt}/{max_attempts} failed: {e}; retrying...");
                std::thread::sleep(Duration::from_secs(attempt as u64 * 2));
                continue;
            }
            Err(e) => {
                return Err(e);
            }
        };

        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            anyhow::bail!(
                "HTTP 401 Unauthorized for {url}\n\n\
                 The requested URL requires authentication.\n\
                 Create a HuggingFace access token at https://huggingface.co/settings/tokens\n\
                 then re-run with your token:\n\
                 \n\
                     sempkg reranker pull --hf-token <YOUR_TOKEN>\n\
                 \n\
                 Or set the environment variable and re-run:\n\
                 \n\
                     $env:HF_TOKEN = \"<YOUR_TOKEN>\"; sempkg reranker pull"
            );
        }

        let mut resp = match resp.error_for_status() {
            Ok(r) => r,
            Err(e) if attempt < max_attempts => {
                println!("  download attempt {attempt}/{max_attempts} failed: {e}; retrying...");
                std::thread::sleep(Duration::from_secs(attempt as u64 * 2));
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
                std::thread::sleep(Duration::from_secs(attempt as u64 * 2));
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp_dest);
                return Err(e);
            }
        }
    }

    anyhow::bail!("download failed for {url}")
}

/// Pull the GGUF model into `~/.sempkg/models/` (or the directory implied
/// by `config`). The tokenizer is embedded inside the GGUF — no separate
/// tokenizer.json download is needed when using llama-cpp-2.
///
/// `hf_token` is forwarded as a `Bearer` authorisation header for gated repos.
pub fn pull_model(
    config: &RerankerConfig,
    hf_token: Option<&str>,
    gguf_url: Option<&str>,
) -> Result<()> {
    let model_path = config.resolved_model_path();
    let source_url = gguf_url.unwrap_or(DEFAULT_GGUF_URL);

    if model_path.is_file() {
        println!("Model already present: {}", model_path.display());
    } else {
        println!("Downloading {}  →  {}", source_url, model_path.display());
        download_file(source_url, &model_path, hf_token)
            .context("Failed to download GGUF model")?;
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
    backend: llama_cpp_2::llama_backend::LlamaBackend,
    model: llama_cpp_2::model::LlamaModel,
    top_k: usize,
    output_n: usize,
}

#[cfg(feature = "reranker")]
impl Reranker {
    /// Load the GGUF model from disk using llama.cpp.
    pub fn load(config: &RerankerConfig) -> Result<Self> {
        use llama_cpp_2::llama_backend::LlamaBackend;
        use llama_cpp_2::model::{params::LlamaModelParams, LlamaModel};
        use llama_cpp_2::{send_logs_to_tracing, LlamaCppError, LogOptions};

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

        // Backend init is process-global and idempotent.
        let backend = match LlamaBackend::init() {
            Ok(b) => b,
            Err(LlamaCppError::BackendAlreadyInitialized) => LlamaBackend {},
            Err(e) => return Err(anyhow::anyhow!("llama backend init: {e}")),
        };

        let model_params = LlamaModelParams::default();
        let model = LlamaModel::load_from_file(&backend, &model_path, &model_params)
            .map_err(|e| anyhow::anyhow!("loading model from {}: {e}", model_path.display()))?;

        Ok(Self {
            backend,
            model,
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
            .with_embeddings(true);

        let mut ctx = self
            .model
            .new_context(&self.backend, ctx_params)
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

    /// Rerank `candidates`, keeping at most `output_n` results.
    pub fn rerank(
        &mut self,
        query: &str,
        candidates: Vec<RerankCandidate>,
    ) -> Result<Vec<RerankResult>> {
        let pool: Vec<RerankCandidate> = candidates.into_iter().take(self.top_k).collect();

        let mut scored: Vec<RerankResult> = pool
            .into_iter()
            .map(|c| {
                let score = self.score_pair(query, &c.text).unwrap_or(0.0);
                RerankResult {
                    source: c.source,
                    text: c.text,
                    origin: c.origin,
                    score,
                }
            })
            .collect();

        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(self.output_n);
        Ok(scored)
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
        &mut self,
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

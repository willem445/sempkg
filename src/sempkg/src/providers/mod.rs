/// Pluggable model provider abstractions for reranking, embedding, and query
/// expansion.
///
/// Three capability traits — [`Embed`], [`Rerank`], [`Expand`] — let the rest
/// of the codebase depend on behaviour rather than concrete types.  Factory
/// functions (`build_*`) read the `[embedding]` / `[reranker]` /
/// `[query_expansion]` config sections and return the right boxed impl:
///
/// - `provider = "local"` (default) → in-process GGUF inference via
///   llama-cpp-2 (requires the `embeddings` / `reranker` cargo feature).
/// - `provider = "openai"` → HTTP calls to any OpenAI-compatible endpoint
///   (OpenAI, OpenRouter, Ollama, LM Studio, vLLM, …).  Always compiled —
///   no llama-cpp toolchain needed.
/// - `provider = "copilot"` → future: Copilot OAuth + Copilot endpoint.
///
/// All three traits are `Send + Sync` so they can be stored in shared
/// context structs without `RefCell`/`Arc<Mutex<…>>` overhead.
use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::query_expansion::ExpandedQuery;
use crate::reranker::{RerankCandidate, RerankResult};

// ---------------------------------------------------------------------------
// Provider kind
// ---------------------------------------------------------------------------

/// Which backend drives a model capability.  Stored in each config section as
/// the `provider` field.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    /// In-process GGUF via llama-cpp-2 (requires the matching cargo feature).
    #[default]
    Local,
    /// Any OpenAI-compatible HTTP endpoint (OpenAI, OpenRouter, Ollama, …).
    OpenAi,
    /// GitHub Copilot endpoint with OAuth token exchange (future phase).
    Copilot,
}

// ---------------------------------------------------------------------------
// Shared OpenAI-compatible provider config
// ---------------------------------------------------------------------------

/// Configuration for `provider = "openai"` (or `"copilot"`) blocks.
///
/// Example in `sempkg.toml`:
/// ```toml
/// [embedding]
/// provider = "openai"
/// [embedding.openai]
/// api_base    = "https://openrouter.ai/api/v1"
/// model       = "openai/text-embedding-3-small"
/// api_key_env = "OPENROUTER_API_KEY"
/// dim         = 1536
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OpenAiProviderConfig {
    /// Base URL of the OpenAI-compatible API endpoint.
    /// Do NOT include a trailing slash.
    pub api_base: String,

    /// Model identifier to send in each request body.
    pub model: String,

    /// Name of the environment variable that holds the API key.
    /// The key value is read at runtime — it is **never** stored in the toml.
    #[serde(default = "default_api_key_env")]
    pub api_key_env: String,

    /// Output dimension of the embedding model.
    /// Required for `[embedding]`; ignored for `[reranker]` / `[query_expansion]`.
    pub dim: Option<usize>,

    /// Per-request timeout in seconds.  Defaults to 120.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_api_key_env() -> String {
    "OPENAI_API_KEY".to_string()
}
fn default_timeout_secs() -> u64 {
    120
}

// ---------------------------------------------------------------------------
// Capability traits
// ---------------------------------------------------------------------------

/// Converts text into dense embedding vectors for semantic search.
pub trait Embed: Send + Sync {
    fn embed_query(&self, query: &str) -> Result<Vec<f32>>;
    #[allow(dead_code)]
    fn embed_document(&self, text: &str) -> Result<Vec<f32>>;
    fn embed_documents_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;
    /// Output dimension of the embedding vectors.
    fn dim(&self) -> usize;
    /// Stable model identifier recorded in index metadata (e.g.
    /// `"qwen3-embedding-0.6b"` or `"openai:text-embedding-3-small"`).
    fn model_id(&self) -> &str;
}

/// Scores (query, document) pairs for reranking.
pub trait Rerank: Send + Sync {
    /// Return P(relevant) ∈ [0, 1] for a single (query, document) pair.
    fn score_pair(&self, query: &str, doc: &str) -> Result<f32>;
    /// Candidate pool size passed to the reranker.
    fn top_k(&self) -> usize;
    /// Number of results to keep after reranking.
    fn output_n(&self) -> usize;

    /// Score `candidates`, truncate to `top_k`, sort descending, and keep at
    /// most `output_n` results.  The default implementation calls `score_pair`
    /// sequentially; override for batch-aware backends.
    fn rerank(&self, query: &str, candidates: Vec<RerankCandidate>) -> Result<Vec<RerankResult>> {
        let pool: Vec<RerankCandidate> = candidates.into_iter().take(self.top_k()).collect();
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
        scored.truncate(self.output_n());
        Ok(scored)
    }
}

/// Rewrites a search query into typed sub-queries for multi-backend retrieval.
pub trait Expand: Send + Sync {
    fn expand(&self, query: &str) -> Vec<ExpandedQuery>;
}

// ---------------------------------------------------------------------------
// Factory — build_embedder
// ---------------------------------------------------------------------------

/// Build the active embedder from `config`.  Returns `None` when the provider
/// is not available (model missing, feature not compiled, bad config).
pub fn build_embedder(config: &crate::embedding::EmbeddingConfig) -> Option<Box<dyn Embed>> {
    if !config.enabled {
        return None;
    }
    match &config.provider {
        ProviderKind::Local => build_local_embedder(config),
        ProviderKind::OpenAi | ProviderKind::Copilot => {
            let oai = config.openai.as_ref()?;
            let dim = oai.dim?;
            match openai::OpenAiEmbedder::new(oai, dim) {
                Ok(e) => Some(Box::new(e)),
                Err(e) => {
                    eprintln!("sempkg: OpenAI embedder init error: {e}");
                    None
                }
            }
        }
    }
}

#[cfg(feature = "embeddings")]
fn build_local_embedder(config: &crate::embedding::EmbeddingConfig) -> Option<Box<dyn Embed>> {
    if !crate::embedding::model_is_present(config) {
        return None;
    }
    match crate::embedding::Embedder::load(config) {
        Ok(e) => Some(Box::new(e)),
        Err(e) => {
            eprintln!("sempkg: embedder load error: {e}");
            None
        }
    }
}

#[cfg(not(feature = "embeddings"))]
fn build_local_embedder(_config: &crate::embedding::EmbeddingConfig) -> Option<Box<dyn Embed>> {
    None
}

// ---------------------------------------------------------------------------
// Factory — build_reranker
// ---------------------------------------------------------------------------

/// Build the active reranker from `config`.
pub fn build_reranker(config: &crate::reranker::RerankerConfig) -> Option<Box<dyn Rerank>> {
    if !config.enabled {
        return None;
    }
    match &config.provider {
        ProviderKind::Local => build_local_reranker(config),
        ProviderKind::OpenAi | ProviderKind::Copilot => {
            let oai = config.openai.as_ref()?;
            match openai::OpenAiReranker::new(oai, config.top_k, config.output_n) {
                Ok(r) => Some(Box::new(r)),
                Err(e) => {
                    eprintln!("sempkg: OpenAI reranker init error: {e}");
                    None
                }
            }
        }
    }
}

#[cfg(feature = "reranker")]
fn build_local_reranker(config: &crate::reranker::RerankerConfig) -> Option<Box<dyn Rerank>> {
    if !crate::reranker::model_is_present(config) {
        return None;
    }
    match crate::reranker::Reranker::load(config) {
        Ok(r) => Some(Box::new(r)),
        Err(e) => {
            eprintln!("sempkg: reranker load error: {e}");
            None
        }
    }
}

#[cfg(not(feature = "reranker"))]
fn build_local_reranker(_config: &crate::reranker::RerankerConfig) -> Option<Box<dyn Rerank>> {
    None
}

// ---------------------------------------------------------------------------
// Factory — build_expander
// ---------------------------------------------------------------------------

/// Build the active query expander from `config`.
pub fn build_expander(
    config: &crate::query_expansion::QueryExpansionConfig,
) -> Option<Box<dyn Expand>> {
    if !config.enabled {
        return None;
    }
    match &config.provider {
        ProviderKind::Local => build_local_expander(config),
        ProviderKind::OpenAi | ProviderKind::Copilot => {
            let oai = config.openai.as_ref()?;
            match openai::OpenAiExpander::new(oai, config.max_variants) {
                Ok(e) => Some(Box::new(e)),
                Err(e) => {
                    eprintln!("sempkg: OpenAI expander init error: {e}");
                    None
                }
            }
        }
    }
}

#[cfg(feature = "embeddings")]
fn build_local_expander(
    config: &crate::query_expansion::QueryExpansionConfig,
) -> Option<Box<dyn Expand>> {
    if !crate::query_expansion::model_is_present(config) {
        return None;
    }
    match crate::query_expansion::QueryExpander::load(config) {
        Ok(e) => Some(Box::new(e)),
        Err(e) => {
            eprintln!("sempkg: expander load error: {e}");
            None
        }
    }
}

#[cfg(not(feature = "embeddings"))]
fn build_local_expander(
    _config: &crate::query_expansion::QueryExpansionConfig,
) -> Option<Box<dyn Expand>> {
    None
}

pub mod openai;

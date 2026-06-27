/// Optional Qwen3-Embedding-0.6B GGUF embedder for vector (semantic) search.
///
/// When built with `--features embeddings` and a model is configured/present,
/// document chunks (code + docs) are embedded at `sempkg embed` time and stored
/// in the LanceDB tables, and queries are embedded at search time so the MCP
/// `query` tool can run vector search in parallel with BM25/FTS.
///
/// **Backend**: llama-cpp-2 (utilityai/llama-cpp-rs), the same crate used by the
/// reranker. The tokenizer is embedded inside the GGUF — no separate
/// tokenizer.json is required.
///
/// **Critical invariant**: the model used to embed documents into a bundle MUST
/// match the model used to embed the query. The model identity ([`EMBED_MODEL_ID`])
/// and dimension are recorded in the index metadata so sempkg can skip vector
/// search when the configured model does not match a bundle's stored vectors.
///
/// When built without `--features embeddings` the inference type compiles to a
/// stub so the rest of the codebase doesn't need `#[cfg]` guards.
use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::reranker::{download_file, expand_tilde};

// ---------------------------------------------------------------------------
// Model identity
// ---------------------------------------------------------------------------

/// Stable identifier recorded in index metadata next to the stored vectors.
/// Vector search is only attempted when the *configured* embedder reports the
/// same id (and dimension) a bundle was embedded with.
pub const EMBED_MODEL_ID: &str = "qwen3-embedding-0.6b";

/// Native embedding dimension of Qwen3-Embedding-0.6B.
pub const EMBED_DIM: usize = 1024;

// ---------------------------------------------------------------------------
// Configuration  (always present — read from [embedding] in sempkg.toml)
// ---------------------------------------------------------------------------

/// Mirrors the `[embedding]` table in `sempkg.toml`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EmbeddingConfig {
    /// Whether vector embedding / vector search is active. Defaults to `true`
    /// when the section exists.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Path to the GGUF model file. May use `~` for the home directory.
    /// Defaults to `~/.sempkg/models/qwen3-embedding-0.6b-q8_0.gguf`.
    pub model: Option<String>,

    /// Context window used when embedding a chunk. Defaults to 2048.
    #[serde(default = "default_n_ctx")]
    pub n_ctx: u32,

    /// Number of model layers to offload to the GPU. `0` (default) = CPU-only.
    /// Requires a llama-cpp-2 build with the matching GPU backend; a CPU-only
    /// build silently ignores any non-zero value.
    #[serde(default)]
    pub gpu_layers: u32,
}

fn default_true() -> bool {
    true
}
fn default_n_ctx() -> u32 {
    2048
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            model: None,
            n_ctx: default_n_ctx(),
            gpu_layers: 0,
        }
    }
}

impl EmbeddingConfig {
    /// Resolve the model path, expanding `~`.
    pub fn resolved_model_path(&self) -> PathBuf {
        let raw = self
            .model
            .clone()
            .unwrap_or_else(|| default_model_path().to_string_lossy().to_string());
        expand_tilde(&raw)
    }
}

/// Default model download directory (shared with the reranker).
pub fn default_model_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".sempkg")
        .join("models")
}

/// Full path to the default GGUF file.
pub fn default_model_path() -> PathBuf {
    default_model_dir().join("qwen3-embedding-0.6b-q8_0.gguf")
}

/// Returns `true` when the GGUF model file exists.
pub fn model_is_present(config: &EmbeddingConfig) -> bool {
    config.resolved_model_path().is_file()
}

/// GGUF download URL — Qwen's official Q8_0 quant of Qwen3-Embedding-0.6B.
/// This repo is public (Apache-2.0) and does not require authentication.
pub const DEFAULT_GGUF_URL: &str =
    "https://huggingface.co/Qwen/Qwen3-Embedding-0.6B-GGUF/resolve/main/Qwen3-Embedding-0.6B-Q8_0.gguf";

/// Pull the GGUF model into `~/.sempkg/models/` (or the directory implied by
/// `config`). `hf_token` is forwarded as a `Bearer` header for gated repos.
pub fn pull_model(
    config: &EmbeddingConfig,
    hf_token: Option<&str>,
    gguf_url: Option<&str>,
) -> Result<()> {
    let model_path = config.resolved_model_path();
    let source_url = gguf_url.unwrap_or(DEFAULT_GGUF_URL);

    if model_path.is_file() {
        println!("Embedding model already present: {}", model_path.display());
    } else {
        println!("Downloading {}  →  {}", source_url, model_path.display());
        download_file(source_url, &model_path, hf_token)?;
        println!("  ✓ embedding model saved.");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Prompt formatting (Qwen3-Embedding instruct style)
// ---------------------------------------------------------------------------

/// Format a query for embedding. Qwen3-Embedding uses an instruction prefix on
/// queries only; documents are embedded as raw text.
pub fn format_query(query: &str) -> String {
    format!("Instruct: Retrieve relevant documents for the given query\nQuery: {query}")
}

/// Format a document/chunk for embedding (raw text, no prefix).
pub fn format_document(text: &str) -> String {
    text.to_string()
}

/// L2-normalize a vector in place so cosine similarity reduces to a dot product
/// and L2 distance ranking matches cosine ranking.
pub fn normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

// ---------------------------------------------------------------------------
// Embedder — full implementation behind the `embeddings` feature flag
// ---------------------------------------------------------------------------

/// Loaded embedder ready to turn text into normalized vectors.
#[cfg(feature = "embeddings")]
pub struct Embedder {
    model: llama_cpp_2::model::LlamaModel,
    n_ctx: u32,
    /// Embedding dimension reported by the loaded model (`n_embd`).
    dim: usize,
}

#[cfg(feature = "embeddings")]
impl Embedder {
    /// Load the GGUF model from disk using llama.cpp.
    pub fn load(config: &EmbeddingConfig) -> Result<Self> {
        use llama_cpp_2::model::{params::LlamaModelParams, LlamaModel};
        use llama_cpp_2::{send_logs_to_tracing, LogOptions};

        // Silence llama.cpp's verbose stderr logging (see reranker.rs).
        static LOG_INIT: std::sync::Once = std::sync::Once::new();
        LOG_INIT.call_once(|| {
            send_logs_to_tracing(LogOptions::default().with_logs_enabled(false));
        });

        let model_path = config.resolved_model_path();
        if !model_path.is_file() {
            anyhow::bail!(
                "Embedding model not found at {}. Run `sempkg embedding pull` to download it.",
                model_path.display()
            );
        }

        // Use the process-wide shared backend (created once, never dropped) so
        // the reranker, embedder, and expander can coexist without a
        // double-free panic in `LlamaBackend::drop` at shutdown.
        let backend = crate::llama_runtime::shared()?;

        let model_params = LlamaModelParams::default().with_n_gpu_layers(config.gpu_layers);
        let model = LlamaModel::load_from_file(backend, &model_path, &model_params)
            .map_err(|e| anyhow::anyhow!("loading model from {}: {e}", model_path.display()))?;

        let dim = model.n_embd() as usize;

        Ok(Self {
            model,
            n_ctx: config.n_ctx,
            dim,
        })
    }

    /// Embedding dimension reported by the loaded model.
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Create an embedding context configured for this embedder.
    fn make_ctx(&self) -> Result<llama_cpp_2::context::LlamaContext<'_>> {
        use llama_cpp_2::context::params::{LlamaContextParams, LlamaPoolingType};
        // Mirror the (working) reranker context setup: only n_ctx + embeddings.
        // Qwen3-Embedding uses last-token pooling, so request it explicitly
        // (the reranker relies on RANK pooling baked into its GGUF metadata).
        let ctx_params = LlamaContextParams::default()
            .with_n_ctx(std::num::NonZeroU32::new(self.n_ctx))
            .with_embeddings(true)
            .with_pooling_type(LlamaPoolingType::Last);
        self.model
            .new_context(crate::llama_runtime::shared()?, ctx_params)
            .map_err(|e| anyhow::anyhow!("creating embedding context: {e}"))
    }

    /// Embed a pre-formatted text string, returning a normalized vector.
    /// Reuses an existing context to avoid repeated KV-cache allocation.
    fn embed_formatted_with_ctx(
        &self,
        ctx: &mut llama_cpp_2::context::LlamaContext<'_>,
        text: &str,
    ) -> Result<Vec<f32>> {
        use llama_cpp_2::{llama_batch::LlamaBatch, model::AddBos};

        let mut tokens = self
            .model
            .str_to_token(text, AddBos::Always)
            .map_err(|e| anyhow::anyhow!("tokenizing text: {e}"))?;

        let n_ctx = ctx.n_ctx() as usize;
        tokens.truncate(n_ctx.max(1));
        if tokens.is_empty() {
            return Ok(vec![0.0; self.dim]);
        }

        let mut batch = LlamaBatch::new(tokens.len(), 1);
        batch
            .add_sequence(&tokens, 0, false)
            .map_err(|e| anyhow::anyhow!("building embed batch: {e}"))?;

        ctx.decode(&mut batch)
            .map_err(|e| anyhow::anyhow!("decoding embed batch: {e}"))?;

        let emb = ctx
            .embeddings_seq_ith(0)
            .map_err(|e| anyhow::anyhow!("reading embedding: {e}"))?;

        let mut v = emb.to_vec();
        normalize(&mut v);
        Ok(v)
    }

    /// Embed a search query (adds the Qwen3-Embedding instruction prefix).
    /// Creates a short-lived context (queries are rare, one at a time).
    pub fn embed_query(&self, query: &str) -> Result<Vec<f32>> {
        let mut ctx = self.make_ctx()?;
        self.embed_formatted_with_ctx(&mut ctx, &format_query(query))
    }

    /// Embed a single document/chunk.
    pub fn embed_document(&self, text: &str) -> Result<Vec<f32>> {
        let mut ctx = self.make_ctx()?;
        self.embed_formatted_with_ctx(&mut ctx, &format_document(text))
    }

    /// Embed a batch of document texts efficiently.
    ///
    /// Creates **one** llama context for the entire operation and decodes each
    /// document as a single sequence (the proven, reliable pattern used by the
    /// reranker), calling `clear_kv_cache()` between decodes. This avoids the
    /// per-row context/KV-cache allocation that dominates `embed_document` while
    /// staying within the single-sequence decode path that pooled embedding
    /// models require.
    pub fn embed_documents_batch(&self, texts: &[impl AsRef<str>]) -> Result<Vec<Vec<f32>>> {
        use llama_cpp_2::{llama_batch::LlamaBatch, model::AddBos};

        if texts.is_empty() {
            return Ok(vec![]);
        }

        // One context reused for every document (model + KV cache allocated once).
        let mut ctx = self.make_ctx()?;
        let n_ctx = ctx.n_ctx() as usize;

        let mut results: Vec<Vec<f32>> = Vec::with_capacity(texts.len());

        for text in texts {
            let mut tokens = self
                .model
                .str_to_token(&format_document(text.as_ref()), AddBos::Always)
                .unwrap_or_default();
            tokens.truncate(n_ctx.max(1));

            if tokens.is_empty() {
                results.push(vec![0.0; self.dim]);
                continue;
            }

            let mut batch = LlamaBatch::new(tokens.len(), 1);
            batch
                .add_sequence(&tokens, 0, false)
                .map_err(|e| anyhow::anyhow!("building embed batch: {e}"))?;

            // Reset KV cache before each decode so sequences don't accumulate.
            ctx.clear_kv_cache();
            ctx.decode(&mut batch)
                .map_err(|e| anyhow::anyhow!("decoding embedding batch: {e}"))?;

            let emb = ctx
                .embeddings_seq_ith(0)
                .map_err(|e| anyhow::anyhow!("reading embedding: {e}"))?;
            let mut v = emb.to_vec();
            normalize(&mut v);
            results.push(v);
        }

        Ok(results)
    }
}

// ---------------------------------------------------------------------------
// No-op stub when the feature is disabled
// ---------------------------------------------------------------------------

/// When compiled *without* `--features embeddings` this is a zero-sized stub.
#[cfg(not(feature = "embeddings"))]
pub struct Embedder;

#[cfg(not(feature = "embeddings"))]
impl Embedder {
    /// Always returns an error explaining the feature is not compiled in.
    pub fn load(_config: &EmbeddingConfig) -> Result<Self> {
        anyhow::bail!(
            "Embedding support is not compiled into this binary. \
             Rebuild with `cargo build --features embeddings`."
        )
    }

    pub fn dim(&self) -> usize {
        EMBED_DIM
    }

    pub fn embed_query(&self, _query: &str) -> Result<Vec<f32>> {
        anyhow::bail!("Embedding support is not compiled into this binary.")
    }

    pub fn embed_document(&self, _text: &str) -> Result<Vec<f32>> {
        anyhow::bail!("Embedding support is not compiled into this binary.")
    }

    pub fn embed_documents_batch(&self, _texts: &[impl AsRef<str>]) -> Result<Vec<Vec<f32>>> {
        anyhow::bail!("Embedding support is not compiled into this binary.")
    }
}

// ---------------------------------------------------------------------------
// Status helper
// ---------------------------------------------------------------------------

pub fn print_status(config: &EmbeddingConfig) {
    let model_path = config.resolved_model_path();

    println!("Embedding configuration:");
    println!("  enabled    : {}", config.enabled);
    println!("  model      : {}", model_path.display());
    println!("  model id   : {EMBED_MODEL_ID}");
    println!("  dimension  : {EMBED_DIM}");
    println!("  n_ctx      : {}", config.n_ctx);
    println!("  gpu_layers : {}", config.gpu_layers);
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

    if model_ok {
        println!();
        println!("Embedding is ready. Run `sempkg embed` to build vector indexes.");
    } else {
        println!();
        println!("Run `sempkg embedding pull` to download the model.");
    }

    #[cfg(not(feature = "embeddings"))]
    {
        println!();
        println!(
            "NOTE: This binary was compiled WITHOUT the `embeddings` feature. \
             Vector search is disabled at runtime even if the model is present. \
             Rebuild with `cargo build --features embeddings` to enable it."
        );
    }
}

/// Optional GGUF embedder for vector (semantic) search.
///
/// Two models are supported via [`EmbeddingModel`]: **EmbeddingGemma-300M**
/// (default, 768-dim, mean pooling) and **Qwen3-Embedding-0.6B** (1024-dim,
/// last-token pooling). The active model is selected by `model_id` in the
/// `[embedding]` section of `sempkg.toml`.
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
/// match the model used to embed the query. The model identity ([`EmbeddingModel::id`])
/// and dimension are recorded in the index metadata so sempkg can skip vector
/// search when the configured model does not match a bundle's stored vectors.
///
/// When built without `--features embeddings` the inference type compiles to a
/// stub so the rest of the codebase doesn't need `#[cfg]` guards.
use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::providers::{Embed, OpenAiProviderConfig, ProviderKind};
use crate::reranker::{download_file, expand_tilde};

// ---------------------------------------------------------------------------
// Model registry
// ---------------------------------------------------------------------------

/// A supported embedding model. Each variant fixes the identity recorded in
/// index metadata, the native dimension, the default GGUF file + download URL,
/// the pooling strategy, and the instruction prompts — everything that must
/// match between the model that embedded a bundle and the model that embeds a
/// query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingModel {
    /// Google EmbeddingGemma-300M (default). 768-dim, mean pooling.
    Gemma300M,
    /// Qwen3-Embedding-0.6B. 1024-dim, last-token pooling.
    Qwen3Embedding0_6B,
}

impl EmbeddingModel {
    /// The model used when none is explicitly configured.
    pub const DEFAULT: EmbeddingModel = EmbeddingModel::Gemma300M;

    /// All selectable `model_id` values, for help text and error messages.
    pub const KNOWN_IDS: &'static [&'static str] = &["embeddinggemma-300m", "qwen3-embedding-0.6b"];

    /// Parse a config `model_id` (case-insensitive, with a few aliases).
    pub fn from_id(id: &str) -> Option<Self> {
        match id.trim().to_ascii_lowercase().as_str() {
            "embeddinggemma-300m" | "embeddinggemma" | "gemma" => Some(Self::Gemma300M),
            "qwen3-embedding-0.6b" | "qwen3-embedding" | "qwen" => Some(Self::Qwen3Embedding0_6B),
            _ => None,
        }
    }

    /// Stable identifier recorded in index metadata next to the stored vectors.
    /// Vector search runs only when the *configured* embedder reports the same
    /// id (and dimension) a bundle was embedded with.
    pub fn id(&self) -> &'static str {
        match self {
            Self::Gemma300M => "embeddinggemma-300m",
            Self::Qwen3Embedding0_6B => "qwen3-embedding-0.6b",
        }
    }

    /// Human-readable display name.
    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Gemma300M => "EmbeddingGemma-300M",
            Self::Qwen3Embedding0_6B => "Qwen3-Embedding-0.6B",
        }
    }

    /// Native embedding dimension.
    pub fn dim(&self) -> usize {
        match self {
            Self::Gemma300M => 768,
            Self::Qwen3Embedding0_6B => 1024,
        }
    }

    /// Default GGUF filename under `~/.sempkg/models/`.
    pub fn default_filename(&self) -> &'static str {
        match self {
            Self::Gemma300M => "embeddinggemma-300m-qat-Q8_0.gguf",
            Self::Qwen3Embedding0_6B => "qwen3-embedding-0.6b-q8_0.gguf",
        }
    }

    /// Public GGUF download URL (no authentication required).
    pub fn gguf_url(&self) -> &'static str {
        match self {
            Self::Gemma300M => "https://huggingface.co/ggml-org/embeddinggemma-300m-qat-q8_0-GGUF/resolve/main/embeddinggemma-300m-qat-Q8_0.gguf",
            Self::Qwen3Embedding0_6B => "https://huggingface.co/Qwen/Qwen3-Embedding-0.6B-GGUF/resolve/main/Qwen3-Embedding-0.6B-Q8_0.gguf",
        }
    }

    /// Pooling strategy the GGUF expects. EmbeddingGemma is a Gemma3 encoder
    /// with **mean** pooling; Qwen3-Embedding uses **last-token** pooling.
    #[cfg(feature = "embeddings")]
    pub fn pooling_type(&self) -> llama_cpp_2::context::params::LlamaPoolingType {
        use llama_cpp_2::context::params::LlamaPoolingType;
        match self {
            Self::Gemma300M => LlamaPoolingType::Mean,
            Self::Qwen3Embedding0_6B => LlamaPoolingType::Last,
        }
    }

    /// Format a search query for embedding (model-specific instruction prefix).
    pub fn format_query(&self, query: &str) -> String {
        match self {
            // EmbeddingGemma retrieval prompt for queries.
            Self::Gemma300M => format!("task: search result | query: {query}"),
            // Qwen3-Embedding instruct prefix (queries only).
            Self::Qwen3Embedding0_6B => {
                format!("Instruct: Retrieve relevant documents for the given query\nQuery: {query}")
            }
        }
    }

    /// Format a document/chunk for embedding.
    pub fn format_document(&self, text: &str) -> String {
        match self {
            // EmbeddingGemma retrieval prompt for documents (no title).
            Self::Gemma300M => format!("title: none | text: {text}"),
            // Qwen3-Embedding embeds documents as raw text.
            Self::Qwen3Embedding0_6B => text.to_string(),
        }
    }
}

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

    /// Which embedding model to use. One of [`EmbeddingModel::KNOWN_IDS`]
    /// (`embeddinggemma-300m` or `qwen3-embedding-0.6b`). Defaults to
    /// `embeddinggemma-300m`. This selects the identity, dimension, pooling,
    /// prompts, and the default GGUF download.
    /// Only used when `provider = "local"`.
    #[serde(default = "default_model_id")]
    pub model_id: String,

    /// Which backend to use. `"local"` (default) requires the `embeddings`
    /// cargo feature; `"openai"` uses any OpenAI-compatible HTTP endpoint.
    #[serde(default)]
    pub provider: ProviderKind,

    /// Optional explicit path to the GGUF file, overriding the default location
    /// for the selected model. May use `~` for the home directory. When set,
    /// make sure it points at the GGUF for `model_id`.
    /// Only used when `provider = "local"`.
    pub model: Option<String>,

    /// HuggingFace (or other) URL to download the GGUF from.
    /// Overrides the built-in default URL when running `sempkg embedding pull`.
    /// Only used when `provider = "local"`.
    pub model_url: Option<String>,

    /// OpenAI-compatible provider settings. Required when `provider = "openai"`.
    /// Must include `dim` (the model's embedding dimension).
    pub openai: Option<OpenAiProviderConfig>,

    /// Context window used when embedding a chunk. Defaults to 2048.
    /// Only used when `provider = "local"`.
    #[serde(default = "default_n_ctx")]
    pub n_ctx: u32,

    /// GPU offload policy: `"auto"` (default), `"on"`, or `"off"`. `auto`/`on`
    /// only take effect when this binary was built with a GPU backend
    /// (`--features embeddings,cuda` or `…,vulkan`); see [`crate::accel::GpuMode`].
    /// Only used when `provider = "local"`.
    #[serde(default)]
    pub gpu: crate::accel::GpuMode,

    /// Number of CPU threads for embedding inference. `0` (default) uses all
    /// logical cores. Threads only matter for layers run on the CPU.
    /// Only used when `provider = "local"`.
    #[serde(default)]
    pub n_threads: u32,

    /// Advanced override: offload exactly this many model layers to the GPU.
    /// `0` (default) defers to `gpu` (auto-detect). A non-zero value forces a
    /// specific partial offload — useful when a small GPU can't hold the whole
    /// model. Requires a GPU-backend build; a CPU-only build ignores it.
    /// Only used when `provider = "local"`.
    #[serde(default)]
    pub gpu_layers: u32,
}

fn default_true() -> bool {
    true
}
fn default_n_ctx() -> u32 {
    2048
}
fn default_model_id() -> String {
    EmbeddingModel::DEFAULT.id().to_string()
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            model_id: default_model_id(),
            provider: ProviderKind::Local,
            model: None,
            model_url: None,
            openai: None,
            n_ctx: default_n_ctx(),
            gpu: crate::accel::GpuMode::default(),
            n_threads: 0,
            gpu_layers: 0,
        }
    }
}

impl EmbeddingConfig {
    /// The selected embedding model. Errors only when `model_id` is set to an
    /// unrecognised value.
    pub fn model(&self) -> Result<EmbeddingModel> {
        EmbeddingModel::from_id(&self.model_id).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown embedding model_id '{}'. Known models: {}",
                self.model_id,
                EmbeddingModel::KNOWN_IDS.join(", ")
            )
        })
    }

    /// Resolve the GGUF path, expanding `~`: the explicit `model` override when
    /// set, otherwise the default file for the selected model.
    pub fn resolved_model_path(&self) -> PathBuf {
        if let Some(raw) = &self.model {
            return expand_tilde(raw);
        }
        let model = self.model().unwrap_or(EmbeddingModel::DEFAULT);
        default_model_dir().join(model.default_filename())
    }
}

/// Default model download directory (shared with the reranker).
pub fn default_model_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".sempkg")
        .join("models")
}

/// Returns `true` when the configured GGUF model file exists.
pub fn model_is_present(config: &EmbeddingConfig) -> bool {
    config.resolved_model_path().is_file()
}

/// Download `model`'s GGUF into `dest`. `gguf_url` overrides the model's default
/// source; `hf_token` is forwarded as a `Bearer` header for gated repos.
pub fn pull_model(
    model: EmbeddingModel,
    dest: &std::path::Path,
    hf_token: Option<&str>,
    gguf_url: Option<&str>,
) -> Result<()> {
    let source_url = gguf_url.unwrap_or_else(|| model.gguf_url());

    if dest.is_file() {
        println!("Embedding model already present: {}", dest.display());
    } else {
        println!("Downloading {}  →  {}", source_url, dest.display());
        download_file(source_url, dest, hf_token)?;
        println!("  ✓ {} saved.", model.display_name());
    }

    Ok(())
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
    /// CPU threads used for inference contexts (resolved: 0 → all cores).
    n_threads: i32,
    /// Embedding dimension reported by the loaded model (`n_embd`).
    dim: usize,
    /// Which model this is — drives pooling, prompts, and the recorded id.
    descriptor: EmbeddingModel,
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

        let descriptor = config.model()?;

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

        let n_gpu_layers =
            crate::accel::resolve_gpu_layers(config.gpu, config.gpu_layers, backend, "embedding");
        let model_params = LlamaModelParams::default().with_n_gpu_layers(n_gpu_layers);
        let model = LlamaModel::load_from_file(backend, &model_path, &model_params)
            .map_err(|e| anyhow::anyhow!("loading model from {}: {e}", model_path.display()))?;

        let dim = model.n_embd() as usize;
        if dim != descriptor.dim() {
            eprintln!(
                "sempkg: warning — {} reports dim {dim} but {} expects {}; using {dim}. \
                 Is `model_id`/`model` pointing at the right GGUF?",
                descriptor.id(),
                descriptor.id(),
                descriptor.dim()
            );
        }

        Ok(Self {
            model,
            n_ctx: config.n_ctx,
            n_threads: crate::accel::resolve_threads(config.n_threads),
            dim,
            descriptor,
        })
    }

    /// Embedding dimension reported by the loaded model.
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Stable identifier of the loaded model (recorded in index metadata).
    pub fn model_id(&self) -> &'static str {
        self.descriptor.id()
    }

    /// Create an embedding context configured for this embedder. The pooling
    /// strategy is model-specific (mean for EmbeddingGemma, last for Qwen3).
    fn make_ctx(&self) -> Result<llama_cpp_2::context::LlamaContext<'_>> {
        use llama_cpp_2::context::params::LlamaContextParams;
        let ctx_params = LlamaContextParams::default()
            .with_n_ctx(std::num::NonZeroU32::new(self.n_ctx))
            .with_embeddings(true)
            .with_pooling_type(self.descriptor.pooling_type())
            .with_n_batch(4096)
            .with_n_ubatch(4096)
            .with_n_threads(self.n_threads)
            .with_n_threads_batch(self.n_threads);
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

    /// Embed a search query (adds the model-specific instruction prefix).
    /// Creates a short-lived context (queries are rare, one at a time).
    pub fn embed_query(&self, query: &str) -> Result<Vec<f32>> {
        let mut ctx = self.make_ctx()?;
        self.embed_formatted_with_ctx(&mut ctx, &self.descriptor.format_query(query))
    }

    /// Embed a single document/chunk.
    pub fn embed_document(&self, text: &str) -> Result<Vec<f32>> {
        let mut ctx = self.make_ctx()?;
        self.embed_formatted_with_ctx(&mut ctx, &self.descriptor.format_document(text))
    }

    /// Embed a batch of document texts efficiently.
    ///
    /// Creates **one** llama context for the entire operation and decodes each
    /// document as a single sequence (the proven, reliable pattern used by the
    /// reranker), calling `clear_kv_cache()` between decodes. This avoids the
    /// per-row context/KV-cache allocation that dominates `embed_document` while
    /// staying within the single-sequence decode path that pooled embedding
    /// models require.
    pub fn embed_documents_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
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
                .str_to_token(
                    &self.descriptor.format_document(text.as_ref()),
                    AddBos::Always,
                )
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

#[cfg(feature = "embeddings")]
impl Embed for Embedder {
    fn embed_query(&self, query: &str) -> Result<Vec<f32>> {
        Embedder::embed_query(self, query)
    }

    fn embed_document(&self, text: &str) -> Result<Vec<f32>> {
        Embedder::embed_document(self, text)
    }

    fn embed_documents_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        Embedder::embed_documents_batch(self, texts)
    }

    fn dim(&self) -> usize {
        Embedder::dim(self)
    }

    fn model_id(&self) -> &str {
        Embedder::model_id(self)
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
        EmbeddingModel::DEFAULT.dim()
    }

    pub fn model_id(&self) -> &'static str {
        EmbeddingModel::DEFAULT.id()
    }

    pub fn embed_query(&self, _query: &str) -> Result<Vec<f32>> {
        anyhow::bail!("Embedding support is not compiled into this binary.")
    }

    pub fn embed_document(&self, _text: &str) -> Result<Vec<f32>> {
        anyhow::bail!("Embedding support is not compiled into this binary.")
    }

    pub fn embed_documents_batch(&self, _texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        anyhow::bail!("Embedding support is not compiled into this binary.")
    }
}

#[cfg(not(feature = "embeddings"))]
impl Embed for Embedder {
    fn embed_query(&self, query: &str) -> Result<Vec<f32>> {
        Embedder::embed_query(self, query)
    }
    fn embed_document(&self, text: &str) -> Result<Vec<f32>> {
        Embedder::embed_document(self, text)
    }
    fn embed_documents_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        Embedder::embed_documents_batch(self, texts)
    }
    fn dim(&self) -> usize {
        Embedder::dim(self)
    }
    fn model_id(&self) -> &str {
        Embedder::model_id(self)
    }
}

// ---------------------------------------------------------------------------
// Status helper
// ---------------------------------------------------------------------------

pub fn print_status(config: &EmbeddingConfig) {
    let selected = config.model();
    let model_path = config.resolved_model_path();

    println!("Embedding configuration:");
    println!("  enabled    : {}", config.enabled);
    match &selected {
        Ok(m) => {
            println!("  model      : {} ({})", m.display_name(), m.id());
            println!("  dimension  : {}", m.dim());
        }
        Err(e) => println!("  model      : <invalid> — {e}"),
    }
    println!("  gguf path  : {}", model_path.display());
    println!("  n_ctx      : {}", config.n_ctx);
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

    println!("Available models (set `model_id` under [embedding] in sempkg.toml):");
    for id in EmbeddingModel::KNOWN_IDS {
        if let Some(m) = EmbeddingModel::from_id(id) {
            let marker = if selected.as_ref().map(|s| *s == m).unwrap_or(false) {
                "→"
            } else {
                " "
            };
            println!("  {marker} {id}  (dim {})", m.dim());
        }
    }
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

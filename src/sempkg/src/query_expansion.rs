/// Optional generative query expansion using a fine-tuned GGUF model.
///
/// Mirrors QMD's approach: a small instruction-tuned model
/// (`qmd-query-expansion-1.7B`) rewrites a short search query into a handful of
/// typed sub-queries — `lex:` (keyword/BM25), `vec:` (semantic), and `hyde:`
/// (hypothetical-answer text for semantic search). sempkg then runs BM25 and
/// vector search for the original query plus each variant and fuses the results
/// with Reciprocal Rank Fusion before reranking.
///
/// **Backend**: llama-cpp-2, behind the same `embeddings` cargo feature as the
/// vector embedder (both are part of the hybrid-search pipeline and share the
/// llama.cpp toolchain).
///
/// **Graceful degradation**: every failure path (feature not compiled, model
/// missing, decode error, empty/garbled output) returns an empty variant list,
/// so the caller transparently falls back to searching the original query only.
use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::reranker::{download_file, expand_tilde};

// ---------------------------------------------------------------------------
// Variant routing
// ---------------------------------------------------------------------------

/// Which retrieval backend an expanded variant should be routed to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpansionKind {
    /// Keyword / full-text (BM25) search.
    Lexical,
    /// Semantic (vector) search. Used for both `vec:` and `hyde:` variants.
    Vector,
}

/// A single expanded sub-query with its routing hint.
#[derive(Debug, Clone)]
pub struct ExpandedQuery {
    pub text: String,
    pub kind: ExpansionKind,
}

// ---------------------------------------------------------------------------
// Configuration  (read from [query_expansion] in sempkg.toml)
// ---------------------------------------------------------------------------

/// Mirrors the `[query_expansion]` table in `sempkg.toml`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct QueryExpansionConfig {
    /// Whether query expansion is active. Defaults to `true` when the section
    /// exists.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Path to the GGUF model file. May use `~`. Defaults to
    /// `~/.sempkg/models/qmd-query-expansion-1.7b-q4_k_m.gguf`.
    pub model: Option<String>,

    /// Maximum number of expanded variants to keep (after parsing/dedup).
    #[serde(default = "default_max_variants")]
    pub max_variants: usize,

    /// Maximum tokens to generate for the expansion.
    #[serde(default = "default_max_tokens")]
    pub max_tokens: i32,

    /// Context window. Defaults to 2048.
    #[serde(default = "default_n_ctx")]
    pub n_ctx: u32,

    /// GPU layers to offload (`0` = CPU-only).
    #[serde(default)]
    pub gpu_layers: u32,

    /// Sampling temperature. Defaults to 0.7 (matches QMD).
    #[serde(default = "default_temperature")]
    pub temperature: f32,
}

fn default_true() -> bool {
    true
}
fn default_max_variants() -> usize {
    4
}
fn default_max_tokens() -> i32 {
    256
}
fn default_n_ctx() -> u32 {
    2048
}
fn default_temperature() -> f32 {
    0.7
}

impl Default for QueryExpansionConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            model: None,
            max_variants: default_max_variants(),
            max_tokens: default_max_tokens(),
            n_ctx: default_n_ctx(),
            gpu_layers: 0,
            temperature: default_temperature(),
        }
    }
}

impl QueryExpansionConfig {
    /// Resolve the model path, expanding `~`.
    pub fn resolved_model_path(&self) -> PathBuf {
        let raw = self
            .model
            .clone()
            .unwrap_or_else(|| default_model_path().to_string_lossy().to_string());
        expand_tilde(&raw)
    }
}

/// Full path to the default GGUF file.
pub fn default_model_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".sempkg")
        .join("models")
        .join("qmd-query-expansion-1.7b-q4_k_m.gguf")
}

/// Returns `true` when the GGUF model file exists.
pub fn model_is_present(config: &QueryExpansionConfig) -> bool {
    config.resolved_model_path().is_file()
}

/// GGUF download URL — QMD's published fine-tune (public repo).
pub const DEFAULT_GGUF_URL: &str =
    "https://huggingface.co/tobil/qmd-query-expansion-1.7B-gguf/resolve/main/qmd-query-expansion-1.7B-q4_k_m.gguf";

/// Pull the GGUF model into `~/.sempkg/models/`.
pub fn pull_model(
    config: &QueryExpansionConfig,
    hf_token: Option<&str>,
    gguf_url: Option<&str>,
) -> Result<()> {
    let model_path = config.resolved_model_path();
    let source_url = gguf_url.unwrap_or(DEFAULT_GGUF_URL);

    if model_path.is_file() {
        println!(
            "Query-expansion model already present: {}",
            model_path.display()
        );
    } else {
        println!("Downloading {}  →  {}", source_url, model_path.display());
        download_file(source_url, &model_path, hf_token)?;
        println!("  ✓ query-expansion model saved.");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Output parsing (shared between the real impl and tests)
// ---------------------------------------------------------------------------

/// Strip a leading `<think>...</think>` block (Qwen3 emits one even with
/// `/no_think`) and return the remaining text.
fn strip_think(text: &str) -> &str {
    let trimmed = text.trim_start();
    if let Some(rest) = trimmed.strip_prefix("<think>") {
        if let Some(end) = rest.find("</think>") {
            return rest[end + "</think>".len()..].trim_start();
        }
    }
    trimmed
}

/// Parse the model's typed-line output into routed variants.
///
/// Each line is expected to look like `lex: ...`, `vec: ...`, or `hyde: ...`.
/// Lines without a recognised prefix are treated leniently as `vec:` variants.
/// Variants are de-duplicated (case-insensitive) against each other and the
/// original query, and capped at `max_variants`.
fn parse_variants(raw: &str, original: &str, max_variants: usize) -> Vec<ExpandedQuery> {
    let body = strip_think(raw);
    let original_norm = original.trim().to_lowercase();

    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    seen.insert(original_norm);

    let mut out: Vec<ExpandedQuery> = Vec::new();

    for line in body.lines() {
        if out.len() >= max_variants {
            break;
        }
        let line = line.trim().trim_start_matches(['-', '*', '•']).trim();
        if line.is_empty() {
            continue;
        }

        let (kind, content) = match line.split_once(':') {
            Some((prefix, rest)) => {
                let p = prefix.trim().to_lowercase();
                match p.as_str() {
                    "lex" => (ExpansionKind::Lexical, rest.trim()),
                    "vec" | "hyde" => (ExpansionKind::Vector, rest.trim()),
                    // Unknown prefix: keep the whole line as a semantic variant.
                    _ => (ExpansionKind::Vector, line),
                }
            }
            // No colon: treat the whole line as a semantic variant.
            None => (ExpansionKind::Vector, line),
        };

        let content = content.trim();
        if content.is_empty() {
            continue;
        }

        let norm = content.to_lowercase();
        if !seen.insert(norm) {
            continue;
        }

        out.push(ExpandedQuery {
            text: content.to_string(),
            kind,
        });
    }

    out
}

/// Build the Qwen3 chat-templated prompt for the expansion model.
fn build_prompt(query: &str) -> String {
    format!(
        "<|im_start|>user\n/no_think Expand this search query: {query}<|im_end|>\n<|im_start|>assistant\n"
    )
}

// ---------------------------------------------------------------------------
// QueryExpander — full implementation behind the `embeddings` feature flag
// ---------------------------------------------------------------------------

#[cfg(feature = "embeddings")]
pub struct QueryExpander {
    backend: llama_cpp_2::llama_backend::LlamaBackend,
    model: llama_cpp_2::model::LlamaModel,
    n_ctx: u32,
    max_tokens: i32,
    temperature: f32,
    max_variants: usize,
}

#[cfg(feature = "embeddings")]
impl QueryExpander {
    /// Load the GGUF model from disk using llama.cpp.
    pub fn load(config: &QueryExpansionConfig) -> Result<Self> {
        use llama_cpp_2::llama_backend::LlamaBackend;
        use llama_cpp_2::model::{params::LlamaModelParams, LlamaModel};
        use llama_cpp_2::{send_logs_to_tracing, LogOptions};

        static LOG_INIT: std::sync::Once = std::sync::Once::new();
        LOG_INIT.call_once(|| {
            send_logs_to_tracing(LogOptions::default().with_logs_enabled(false));
        });

        let model_path = config.resolved_model_path();
        if !model_path.is_file() {
            anyhow::bail!(
                "Query-expansion model not found at {}. Run `sempkg query-expansion pull`.",
                model_path.display()
            );
        }

        let backend =
            LlamaBackend::init().map_err(|e| anyhow::anyhow!("llama backend init: {e}"))?;

        let model_params = LlamaModelParams::default().with_n_gpu_layers(config.gpu_layers);
        let model = LlamaModel::load_from_file(&backend, &model_path, &model_params)
            .map_err(|e| anyhow::anyhow!("loading model from {}: {e}", model_path.display()))?;

        Ok(Self {
            backend,
            model,
            n_ctx: config.n_ctx,
            max_tokens: config.max_tokens,
            temperature: config.temperature,
            max_variants: config.max_variants,
        })
    }

    /// Expand `query` into routed variants. Returns an empty vector on any
    /// failure so the caller can fall back to the original query.
    pub fn expand(&self, query: &str) -> Vec<ExpandedQuery> {
        match self.generate(query) {
            Ok(raw) => parse_variants(&raw, query, self.max_variants),
            Err(_) => Vec::new(),
        }
    }

    /// Run greedy-ish sampling to produce the raw model output.
    #[allow(deprecated)]
    fn generate(&self, query: &str) -> Result<String> {
        use llama_cpp_2::context::params::LlamaContextParams;
        use llama_cpp_2::llama_batch::LlamaBatch;
        use llama_cpp_2::model::{AddBos, Special};
        use llama_cpp_2::sampling::LlamaSampler;

        let prompt = build_prompt(query);

        let ctx_params =
            LlamaContextParams::default().with_n_ctx(std::num::NonZeroU32::new(self.n_ctx));

        let mut ctx = self
            .model
            .new_context(&self.backend, ctx_params)
            .map_err(|e| anyhow::anyhow!("creating context: {e}"))?;

        let tokens = self
            .model
            .str_to_token(&prompt, AddBos::Always)
            .map_err(|e| anyhow::anyhow!("tokenizing prompt: {e}"))?;

        let n_ctx = ctx.n_ctx() as usize;
        if tokens.len() >= n_ctx {
            anyhow::bail!("prompt exceeds context window");
        }

        // Decode the prompt. The batch needs capacity for the prompt and the
        // single tokens fed back during generation.
        let mut batch = LlamaBatch::new(tokens.len().max(1), 1);
        batch
            .add_sequence(&tokens, 0, false)
            .map_err(|e| anyhow::anyhow!("building batch: {e}"))?;
        ctx.decode(&mut batch)
            .map_err(|e| anyhow::anyhow!("decoding prompt: {e}"))?;

        let mut sampler = LlamaSampler::chain_simple([
            LlamaSampler::top_k(20),
            LlamaSampler::top_p(0.8, 1),
            LlamaSampler::temp(self.temperature),
            LlamaSampler::dist(0),
        ]);

        let mut out = String::new();
        let prompt_len = tokens.len() as i32;
        // Index into the most-recently-decoded batch whose logits we sample.
        let mut logits_idx = batch.n_tokens() - 1;
        let mut step_batch = LlamaBatch::new(1, 1);

        for step in 0..self.max_tokens {
            let token = sampler.sample(&ctx, logits_idx);
            sampler.accept(token);

            if self.model.is_eog_token(token) {
                break;
            }

            if let Ok(piece) = self.model.token_to_str(token, Special::Plaintext) {
                out.push_str(&piece);
                // Early exit once we clearly have enough lines.
                if out.matches('\n').count() > self.max_variants + 2 {
                    break;
                }
            }

            // Absolute KV-cache position of this generated token.
            let n_cur = prompt_len + step;
            step_batch.clear();
            step_batch
                .add(token, n_cur, &[0], true)
                .map_err(|e| anyhow::anyhow!("adding token to batch: {e}"))?;
            ctx.decode(&mut step_batch)
                .map_err(|e| anyhow::anyhow!("decoding token: {e}"))?;
            logits_idx = 0;
        }

        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// No-op stub when the feature is disabled
// ---------------------------------------------------------------------------

#[cfg(not(feature = "embeddings"))]
pub struct QueryExpander;

#[cfg(not(feature = "embeddings"))]
impl QueryExpander {
    pub fn load(_config: &QueryExpansionConfig) -> Result<Self> {
        anyhow::bail!(
            "Query expansion is not compiled into this binary. \
             Rebuild with `cargo build --features embeddings`."
        )
    }

    /// Always returns no variants (caller falls back to the original query).
    pub fn expand(&self, _query: &str) -> Vec<ExpandedQuery> {
        Vec::new()
    }
}

// ---------------------------------------------------------------------------
// Status helper
// ---------------------------------------------------------------------------

pub fn print_status(config: &QueryExpansionConfig) {
    let model_path = config.resolved_model_path();

    println!("Query-expansion configuration:");
    println!("  enabled      : {}", config.enabled);
    println!("  model        : {}", model_path.display());
    println!("  max_variants : {}", config.max_variants);
    println!("  max_tokens   : {}", config.max_tokens);
    println!("  n_ctx        : {}", config.n_ctx);
    println!("  gpu_layers   : {}", config.gpu_layers);
    println!("  temperature  : {}", config.temperature);
    println!();

    let model_ok = model_path.is_file();
    println!(
        "  model file   : {}",
        if model_ok {
            "✓ present"
        } else {
            "✗ missing"
        }
    );

    if !model_ok {
        println!();
        println!("Run `sempkg query-expansion pull` to download the model.");
    }

    #[cfg(not(feature = "embeddings"))]
    {
        println!();
        println!(
            "NOTE: This binary was compiled WITHOUT the `embeddings` feature. \
             Query expansion is disabled at runtime. \
             Rebuild with `cargo build --features embeddings` to enable it."
        );
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_think_block() {
        let s = "<think>\n\n</think>\nlex: hello world";
        assert_eq!(strip_think(s), "lex: hello world");
    }

    #[test]
    fn parses_typed_lines() {
        let raw =
            "lex: tokio runtime\nvec: async executor internals\nhyde: Tokio is an async runtime\n";
        let v = parse_variants(raw, "tokio runtime spawn", 10);
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].kind, ExpansionKind::Lexical);
        assert_eq!(v[1].kind, ExpansionKind::Vector);
        assert_eq!(v[2].kind, ExpansionKind::Vector);
    }

    #[test]
    fn dedups_and_caps() {
        let raw = "vec: alpha\nvec: alpha\nvec: beta\nvec: gamma";
        let v = parse_variants(raw, "query", 2);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].text, "alpha");
        assert_eq!(v[1].text, "beta");
    }

    #[test]
    fn skips_original_query_duplicate() {
        let raw = "lex: same query\nvec: different";
        let v = parse_variants(raw, "same query", 10);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].text, "different");
    }
}

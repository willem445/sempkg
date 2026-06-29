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

use crate::providers::{Expand, OpenAiProviderConfig, ProviderKind};
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

    /// Which backend to use. `"local"` (default) requires the `embeddings`
    /// cargo feature; `"openai"` uses any OpenAI-compatible HTTP endpoint.
    #[serde(default)]
    pub provider: ProviderKind,

    /// Path to the GGUF model file. May use `~`. Defaults to
    /// `~/.sempkg/models/qmd-query-expansion-1.7b-q4_k_m.gguf`.
    /// Only used when `provider = "local"`.
    pub model: Option<String>,

    /// HuggingFace (or other) URL to download the GGUF from.
    /// Overrides the built-in default URL when pulling the model.
    /// Only used when `provider = "local"`.
    pub model_url: Option<String>,

    /// OpenAI-compatible provider settings. Required when `provider = "openai"`.
    pub openai: Option<OpenAiProviderConfig>,

    /// Maximum number of expanded variants to keep (after parsing/dedup).
    #[serde(default = "default_max_variants")]
    pub max_variants: usize,

    /// Maximum tokens to generate for the expansion.
    #[serde(default = "default_max_tokens")]
    pub max_tokens: i32,

    /// Context window. Defaults to 2048.
    #[serde(default = "default_n_ctx")]
    pub n_ctx: u32,

    /// GPU offload policy: `"auto"` (default), `"on"`, or `"off"`. Only takes
    /// effect on a GPU-backend build; see [`crate::accel::GpuMode`].
    #[serde(default)]
    pub gpu: crate::accel::GpuMode,

    /// CPU threads for inference. `0` (default) uses all logical cores.
    #[serde(default)]
    pub n_threads: u32,

    /// Advanced override: offload exactly this many model layers to the GPU.
    /// `0` (default) defers to `gpu` (auto-detect). A non-zero value forces a
    /// specific partial offload. Requires a GPU-backend build.
    #[serde(default)]
    pub gpu_layers: u32,

    /// Sampling temperature. Defaults to 0.7 (matches QMD).
    #[serde(default = "default_temperature")]
    pub temperature: f32,

    /// When true (default), expanded variants may only **reinforce** documents
    /// the original query already retrieved — a document found *solely* by an
    /// expansion variant is dropped before pool selection. This is the primary
    /// guard against generic variants ("best practices for …") introducing and
    /// then dominating off-topic candidates. When false, expansion-introduced
    /// documents are admitted but capped by `max_expansion_pool_fraction`.
    #[serde(default = "default_true")]
    pub additive_only: bool,

    /// When `additive_only` is false, the maximum fraction of the candidate
    /// pool that expansion-*introduced* (not original-found) documents may
    /// occupy. Keeps expansion strictly non–pool-dominating even when it is
    /// allowed to contribute novel documents. Clamped to `[0.0, 1.0]`.
    #[serde(default = "default_expansion_pool_fraction")]
    pub max_expansion_pool_fraction: f32,

    /// Enable topical anchoring during cross-package fusion. Hits in packages
    /// the original query did not surface are down-weighted toward
    /// `anchor_floor`, so a bare lexical match in an unrelated package cannot
    /// outrank an on-topic semantic match in the right one.
    #[serde(default = "default_true")]
    pub topical_anchoring: bool,

    /// Lowest anchor multiplier applied to a package the original query never
    /// retrieved. `1.0` disables the effect; lower values penalise harder.
    /// Clamped to `[0.0, 1.0]`.
    #[serde(default = "default_anchor_floor")]
    pub anchor_floor: f32,
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
fn default_expansion_pool_fraction() -> f32 {
    0.34
}
fn default_anchor_floor() -> f32 {
    0.5
}

impl Default for QueryExpansionConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            provider: ProviderKind::Local,
            model: None,
            model_url: None,
            openai: None,
            max_variants: default_max_variants(),
            max_tokens: default_max_tokens(),
            n_ctx: default_n_ctx(),
            gpu: crate::accel::GpuMode::default(),
            n_threads: 0,
            gpu_layers: 0,
            temperature: default_temperature(),
            additive_only: default_true(),
            max_expansion_pool_fraction: default_expansion_pool_fraction(),
            topical_anchoring: default_true(),
            anchor_floor: default_anchor_floor(),
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
    // Priority: CLI --gguf-url flag > toml model_url > built-in default
    let source_url = gguf_url
        .or_else(|| config.model_url.as_deref())
        .unwrap_or(DEFAULT_GGUF_URL);

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

/// Function words that carry no topical signal. Used both to extract the
/// *content* tokens of a query/variant and to detect dangling trailing words.
/// Kept deliberately small — domain identifiers (`fs`, `io`, `wait`) must NOT
/// appear here, only natural-language glue and the generative-cliché filler the
/// expansion model is prone to emit.
const STOPWORDS: &[&str] = &[
    "a",
    "an",
    "and",
    "the",
    "to",
    "of",
    "for",
    "with",
    "in",
    "on",
    "at",
    "by",
    "from",
    "as",
    "into",
    "than",
    "that",
    "this",
    "these",
    "those",
    "or",
    "nor",
    "but",
    "is",
    "are",
    "be",
    "how",
    "what",
    "when",
    "where",
    "which",
    "who",
    "why",
    "do",
    "does",
    "using",
    "use",
    "via",
    "about",
    "best",
    "practices",
    "practice",
    "recommended",
    "approach",
    "approaches",
    "common",
    "general",
    "way",
    "ways",
    "guide",
    "tutorial",
    "example",
    "examples",
    "overview",
    "introduction",
    "tips",
];

/// Words that must not be the *last* token of a usable variant. A query ending
/// in one of these is a truncated fragment ("expand the search for", "abort
/// communication with") that matches boilerplate everywhere.
const TRAILING_BANNED: &[&str] = &[
    "a", "an", "the", "to", "of", "for", "with", "in", "on", "at", "by", "from", "as", "into",
    "than", "that", "and", "or", "nor", "but", "is", "are", "be", "how", "what", "when", "which",
    "this", "these", "those", "using", "via", "about",
];

/// Minimum character length (trimmed) for a variant to be considered usable.
const MIN_VARIANT_CHARS: usize = 4;

/// Lowercase alphanumeric content tokens of `s`, with function words removed.
fn content_tokens(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_ascii_lowercase())
        .filter(|t| !STOPWORDS.contains(&t.as_str()))
        .collect()
}

/// Decide whether an expansion `variant` is safe to add for `original`.
///
/// Rejects three failure modes that poison the candidate pool with generic,
/// content-free matches (see `docs/design/query-tool-design.md`):
/// 1. **Too short** — empty, sub-`MIN_VARIANT_CHARS`, or no content tokens.
/// 2. **Dangling trailing word** — ends in a preposition/conjunction/article
///    (e.g. "best practices for"), i.e. a truncated fragment.
/// 3. **Topically unrelated** — shares no content token with the original
///    query, i.e. a query-reformulation cliché rather than a paraphrase.
///
/// When the original has no content tokens of its own (all function words),
/// the overlap check is skipped — there is nothing to anchor against.
fn variant_is_acceptable(variant: &str, original_tokens: &[String]) -> bool {
    let trimmed = variant.trim();
    if trimmed.chars().count() < MIN_VARIANT_CHARS {
        return false;
    }

    // (b) dangling trailing preposition/conjunction/article.
    if let Some(last) = trimmed
        .split(|c: char| !c.is_alphanumeric())
        .rfind(|t| !t.is_empty())
    {
        if TRAILING_BANNED.contains(&last.to_ascii_lowercase().as_str()) {
            return false;
        }
    }

    let variant_tokens = content_tokens(trimmed);
    // (a) no content tokens at all → pure filler.
    if variant_tokens.is_empty() {
        return false;
    }

    // (c) share at least one content token with the original (when the
    // original has any). Anchors the variant to the query's domain.
    if !original_tokens.is_empty() && !variant_tokens.iter().any(|t| original_tokens.contains(t)) {
        return false;
    }

    true
}

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
pub(crate) fn parse_variants(raw: &str, original: &str, max_variants: usize) -> Vec<ExpandedQuery> {
    let body = strip_think(raw);
    let original_norm = original.trim().to_lowercase();
    let original_tokens = content_tokens(original);

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

        // Guard: drop generic / fragmentary / topically-unrelated variants
        // before they can flood the candidate pool with content-free matches.
        if !variant_is_acceptable(content, &original_tokens) {
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
///
/// The instruction steers the model toward **domain paraphrases** of the
/// query's own concepts and away from generative query-reformulation clichés
/// ("best practices for …", "recommended approaches to …"). Those fillers match
/// boilerplate in every package and crowd the on-topic answer out of the pool,
/// so the prompt explicitly bans them and requires each variant to reuse the
/// query's domain terms or a close synonym. Output stays in the typed-line
/// format (`lex:` / `vec:` / `hyde:`) the parser expects.
fn build_prompt(query: &str) -> String {
    format!(
        "<|im_start|>system\nYou rewrite a code/documentation search query into a few \
         search variants that preserve its specific technical meaning. Each variant MUST \
         reuse the query's domain terms (identifiers, API names, concepts) or a precise \
         synonym. Do NOT add generic filler such as \"best practices\", \"recommended \
         approaches\", \"how to\", \"overview\", or \"examples\". Do NOT broaden the topic. \
         Output one variant per line, each prefixed with `lex:` (keyword terms), `vec:` (a \
         semantic paraphrase), or `hyde:` (one sentence a matching doc/comment would \
         contain). No prose, no numbering.<|im_end|>\n\
         <|im_start|>user\n/no_think Expand this search query: {query}<|im_end|>\n\
         <|im_start|>assistant\n"
    )
}

// ---------------------------------------------------------------------------
// QueryExpander — full implementation behind the `embeddings` feature flag
// ---------------------------------------------------------------------------

#[cfg(feature = "embeddings")]
pub struct QueryExpander {
    model: llama_cpp_2::model::LlamaModel,
    n_ctx: u32,
    /// CPU threads used for the generation context (resolved: 0 → all cores).
    n_threads: i32,
    max_tokens: i32,
    temperature: f32,
    max_variants: usize,
}

#[cfg(feature = "embeddings")]
impl QueryExpander {
    /// Load the GGUF model from disk using llama.cpp.
    pub fn load(config: &QueryExpansionConfig) -> Result<Self> {
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

        // Use the process-wide shared backend (created once, never dropped) so
        // the expander can coexist with the embedder/reranker without a
        // double-free panic in `LlamaBackend::drop` at shutdown.
        let backend = crate::llama_runtime::shared()?;

        let n_gpu_layers = crate::accel::resolve_gpu_layers(
            config.gpu,
            config.gpu_layers,
            backend,
            "query_expansion",
        );
        let model_params = LlamaModelParams::default().with_n_gpu_layers(n_gpu_layers);
        let model = LlamaModel::load_from_file(backend, &model_path, &model_params)
            .map_err(|e| anyhow::anyhow!("loading model from {}: {e}", model_path.display()))?;

        Ok(Self {
            model,
            n_ctx: config.n_ctx,
            n_threads: crate::accel::resolve_threads(config.n_threads),
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

        let ctx_params = LlamaContextParams::default()
            .with_n_ctx(std::num::NonZeroU32::new(self.n_ctx))
            .with_n_threads(self.n_threads)
            .with_n_threads_batch(self.n_threads);

        let mut ctx = self
            .model
            .new_context(crate::llama_runtime::shared()?, ctx_params)
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

#[cfg(feature = "embeddings")]
impl Expand for QueryExpander {
    fn expand(&self, query: &str) -> Vec<ExpandedQuery> {
        QueryExpander::expand(self, query)
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

#[cfg(not(feature = "embeddings"))]
impl Expand for QueryExpander {
    fn expand(&self, query: &str) -> Vec<ExpandedQuery> {
        QueryExpander::expand(self, query)
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
    println!(
        "  cpu threads  : {} ({})",
        crate::accel::resolve_threads(config.n_threads),
        if config.n_threads == 0 {
            "all cores"
        } else {
            "configured"
        }
    );
    println!(
        "  gpu          : {}{}",
        config.gpu.as_str(),
        if config.gpu_layers > 0 {
            format!(" (manual override: {} layers)", config.gpu_layers)
        } else {
            String::new()
        }
    );
    println!("  gpu build    : {}", crate::accel::gpu_build_status());
    println!("  temperature  : {}", config.temperature);
    println!("  additive_only: {}", config.additive_only);
    println!(
        "  max_expansion_pool_fraction : {}",
        config.max_expansion_pool_fraction
    );
    println!("  topical_anchoring : {}", config.topical_anchoring);
    println!("  anchor_floor : {}", config.anchor_floor);
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
        // Every variant shares a domain token with the original so the topical
        // guard keeps them; the test exercises lex/vec/hyde routing.
        let raw =
            "lex: tokio runtime\nvec: async executor internals\nhyde: Tokio is an async runtime\n";
        let v = parse_variants(raw, "tokio async runtime executor spawn", 10);
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].kind, ExpansionKind::Lexical);
        assert_eq!(v[1].kind, ExpansionKind::Vector);
        assert_eq!(v[2].kind, ExpansionKind::Vector);
    }

    #[test]
    fn dedups_and_caps() {
        let raw = "vec: alpha\nvec: alpha\nvec: beta\nvec: gamma";
        let v = parse_variants(raw, "alpha beta gamma delta", 2);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].text, "alpha");
        assert_eq!(v[1].text, "beta");
    }

    #[test]
    fn skips_original_query_duplicate() {
        let raw = "lex: same query\nvec: different query";
        let v = parse_variants(raw, "same query", 10);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].text, "different query");
    }

    #[test]
    fn rejects_generic_filler_with_no_overlap() {
        // The exact regression: the expander emits query-reformulation clichés
        // that share no content token with the original. All must be dropped.
        let raw =
            "lex: best practices for\nvec: recommended approaches\nhyde: common ways to use this";
        let v = parse_variants(raw, "abort communication with FS unit", 10);
        assert!(v.is_empty(), "generic filler must not survive: {v:?}");
    }

    #[test]
    fn rejects_dangling_trailing_preposition() {
        // Shares a token ("communication") but ends in a dangling preposition.
        let raw = "lex: abort communication with";
        let v = parse_variants(raw, "abort communication with FS unit", 10);
        assert!(v.is_empty(), "dangling fragment must be dropped: {v:?}");
    }

    #[test]
    fn rejects_too_short_variant() {
        let raw = "vec: io\nlex: a";
        let v = parse_variants(raw, "io completion port wait", 10);
        // "io" overlaps but is < MIN_VARIANT_CHARS; "a" is a stopword fragment.
        assert!(v.is_empty(), "too-short variants must be dropped: {v:?}");
    }

    #[test]
    fn keeps_on_topic_domain_paraphrase() {
        // A genuine domain paraphrase that reuses a query term survives.
        let raw = "lex: wait_for_index completion\nvec: block until the index is ready\nhyde: the call waits for index ingestion to finish";
        let v = parse_variants(raw, "wait for index ready", 10);
        assert_eq!(v.len(), 3, "on-topic paraphrases must survive: {v:?}");
        assert_eq!(v[0].kind, ExpansionKind::Lexical);
    }

    #[test]
    fn overlap_check_skipped_when_original_has_no_content_tokens() {
        // Original is all stopwords → nothing to anchor against, so the
        // overlap guard is bypassed (length / dangling guards still apply).
        let raw = "vec: tokio async runtime";
        let v = parse_variants(raw, "how to do this", 10);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].text, "tokio async runtime");
    }
}

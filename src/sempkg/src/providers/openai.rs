/// OpenAI-compatible HTTP provider implementations.
///
/// `OpenAiEmbedder`, `OpenAiReranker`, and `OpenAiExpander` each talk to any
/// API that speaks the OpenAI wire format — OpenAI itself, OpenRouter, Ollama,
/// LM Studio, vLLM, etc.  They are **always compiled** (no native toolchain
/// required) and selected when `provider = "openai"` appears in the relevant
/// `sempkg.toml` section.
///
/// API keys are read from the environment variable named in `api_key_env` and
/// are never stored in toml.  If the variable is unset the call fails with a
/// clear error.
///
/// Reranking is implemented via **chat-completion scoring**: the model is asked
/// to judge relevance in a yes/no format, and P(yes) is derived from the
/// response (logprobs when available, otherwise parsed numeric reply).  This
/// works with any OpenAI-compatible chat endpoint.
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::json;

use super::{Embed, Expand, OpenAiProviderConfig, Rerank};
use crate::query_expansion::{parse_variants, ExpandedQuery};

// ---------------------------------------------------------------------------
// Shared HTTP client helper
// ---------------------------------------------------------------------------

fn make_client(timeout_secs: u64) -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .connect_timeout(Duration::from_secs(30))
        .build()
        .context("building HTTP client")
}

fn resolve_api_key(cfg: &OpenAiProviderConfig) -> Result<String> {
    std::env::var(&cfg.api_key_env).with_context(|| {
        format!(
            "OpenAI provider requires the `{}` environment variable to be set \
             (api_key_env in [*.openai] config block). \
             Set it to your API key and retry.",
            cfg.api_key_env
        )
    })
}

// ---------------------------------------------------------------------------
// Embed
// ---------------------------------------------------------------------------

/// Calls `POST {api_base}/embeddings` for each batch.
pub struct OpenAiEmbedder {
    client: reqwest::blocking::Client,
    api_base: String,
    model: String,
    api_key: String,
    dim: usize,
    model_id: String,
}

impl OpenAiEmbedder {
    pub fn new(cfg: &OpenAiProviderConfig, dim: usize) -> Result<Self> {
        let api_key = resolve_api_key(cfg)?;
        let client = make_client(cfg.timeout_secs)?;
        let model_id = format!("openai:{}", cfg.model);
        Ok(Self {
            client,
            api_base: cfg.api_base.trim_end_matches('/').to_string(),
            model: cfg.model.clone(),
            api_key,
            dim,
            model_id,
        })
    }

    fn call_embeddings(&self, inputs: Vec<String>) -> Result<Vec<Vec<f32>>> {
        #[derive(Deserialize)]
        struct EmbData {
            embedding: Vec<f32>,
        }
        #[derive(Deserialize)]
        struct EmbResponse {
            data: Vec<EmbData>,
        }

        let url = format!("{}/embeddings", self.api_base);
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&json!({ "model": self.model, "input": inputs }))
            .send()
            .with_context(|| format!("POST {url}"))?
            .error_for_status()
            .with_context(|| format!("embeddings request to {url}"))?;

        let body: EmbResponse = resp.json().context("parsing embeddings response")?;
        Ok(body.data.into_iter().map(|d| d.embedding).collect())
    }
}

impl Embed for OpenAiEmbedder {
    fn embed_query(&self, query: &str) -> Result<Vec<f32>> {
        let vecs = self.call_embeddings(vec![query.to_string()])?;
        vecs.into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("empty embeddings response"))
    }

    fn embed_document(&self, text: &str) -> Result<Vec<f32>> {
        let vecs = self.call_embeddings(vec![text.to_string()])?;
        vecs.into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("empty embeddings response"))
    }

    fn embed_documents_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        self.call_embeddings(texts.iter().map(|s| s.to_string()).collect())
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }
}

// ---------------------------------------------------------------------------
// Rerank
// ---------------------------------------------------------------------------

/// Chat-completion-based reranker — works with any OpenAI-compatible endpoint.
///
/// For each (query, document) pair the model is asked to respond "yes" or "no"
/// to a relevance question.  When the API returns `logprobs`, the log
/// probability of "yes" / "Yes" / "YES" tokens is exponentiated and used
/// directly.  Otherwise the model's text reply is parsed for a numeric score
/// or a yes/no answer.
pub struct OpenAiReranker {
    client: reqwest::blocking::Client,
    api_base: String,
    model: String,
    api_key: String,
    top_k: usize,
    output_n: usize,
}

impl OpenAiReranker {
    pub fn new(cfg: &OpenAiProviderConfig, top_k: usize, output_n: usize) -> Result<Self> {
        let api_key = resolve_api_key(cfg)?;
        let client = make_client(cfg.timeout_secs)?;
        Ok(Self {
            client,
            api_base: cfg.api_base.trim_end_matches('/').to_string(),
            model: cfg.model.clone(),
            api_key,
            top_k,
            output_n,
        })
    }

    fn chat_score(&self, query: &str, document: &str) -> Result<f32> {
        #[allow(dead_code)]
        #[derive(Deserialize)]
        struct TopLogprob {
            token: String,
            logprob: f64,
        }
        #[allow(dead_code)]
        #[derive(Deserialize)]
        struct LogprobContent {
            top_logprobs: Option<Vec<TopLogprob>>,
        }
        #[derive(Deserialize)]
        struct Choice {
            message: serde_json::Value,
            logprobs: Option<serde_json::Value>,
        }
        #[derive(Deserialize)]
        struct ChatResponse {
            choices: Vec<Choice>,
        }

        let system = "You are a relevance judge. Answer only \"yes\" if the Document answers \
                      the Query, or \"no\" if it does not. No other text.";
        let user = format!(
            "Query: {query}\n\nDocument:\n{document}\n\nDoes this document answer the query? (yes/no)"
        );

        let url = format!("{}/chat/completions", self.api_base);
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&json!({
                "model": self.model,
                "messages": [
                    { "role": "system", "content": system },
                    { "role": "user",   "content": user  }
                ],
                "max_tokens": 5,
                "temperature": 0.0,
                "logprobs": true,
                "top_logprobs": 5
            }))
            .send()
            .with_context(|| format!("POST {url}"))?
            .error_for_status()
            .with_context(|| format!("chat completions request to {url}"))?;

        let body: ChatResponse = resp.json().context("parsing chat response")?;
        let choice = body
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("empty choices in chat response"))?;

        // Try logprobs path first (most accurate).
        if let Some(lp_val) = &choice.logprobs {
            if let Some(content_arr) = lp_val.get("content").and_then(|v| v.as_array()) {
                if let Some(first) = content_arr.first() {
                    if let Some(top) = first.get("top_logprobs").and_then(|v| v.as_array()) {
                        let yes_logprob: Option<f64> = top.iter().find_map(|entry| {
                            let tok = entry.get("token")?.as_str()?;
                            let lp = entry.get("logprob")?.as_f64()?;
                            let tok_lower = tok.trim().to_lowercase();
                            if tok_lower.starts_with("yes") {
                                Some(lp)
                            } else {
                                None
                            }
                        });
                        let no_logprob: Option<f64> = top.iter().find_map(|entry| {
                            let tok = entry.get("token")?.as_str()?;
                            let lp = entry.get("logprob")?.as_f64()?;
                            let tok_lower = tok.trim().to_lowercase();
                            if tok_lower.starts_with("no") {
                                Some(lp)
                            } else {
                                None
                            }
                        });
                        if let (Some(y), Some(n)) = (yes_logprob, no_logprob) {
                            let yes_prob = y.exp();
                            let no_prob = n.exp();
                            let sum = yes_prob + no_prob;
                            if sum > 0.0 {
                                return Ok((yes_prob / sum) as f32);
                            }
                        } else if let Some(y) = yes_logprob {
                            return Ok(y.exp().min(1.0) as f32);
                        }
                    }
                }
            }
        }

        // Fall back to text parsing.
        let text = choice
            .message
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_lowercase();

        if text.starts_with("yes") {
            return Ok(1.0);
        }
        if text.starts_with("no") {
            return Ok(0.0);
        }
        // Try to parse a numeric score.
        if let Ok(score) = text.parse::<f32>() {
            return Ok(score.clamp(0.0, 1.0));
        }

        // Unknown response — treat as not relevant.
        Ok(0.0)
    }
}

impl Rerank for OpenAiReranker {
    fn score_pair(&self, query: &str, doc: &str) -> Result<f32> {
        self.chat_score(query, doc)
    }

    fn top_k(&self) -> usize {
        self.top_k
    }

    fn output_n(&self) -> usize {
        self.output_n
    }
}

// ---------------------------------------------------------------------------
// Expand
// ---------------------------------------------------------------------------

/// Query expander backed by a chat-completion model.
///
/// Uses the same output format as the local GGUF model so the existing
/// `parse_variants()` parser can be reused:
/// ```
/// lex: <keyword variant>
/// vec: <semantic variant>
/// hyde: <hypothetical document excerpt>
/// ```
pub struct OpenAiExpander {
    client: reqwest::blocking::Client,
    api_base: String,
    model: String,
    api_key: String,
    max_variants: usize,
}

impl OpenAiExpander {
    pub fn new(cfg: &OpenAiProviderConfig, max_variants: usize) -> Result<Self> {
        let api_key = resolve_api_key(cfg)?;
        let client = make_client(cfg.timeout_secs)?;
        Ok(Self {
            client,
            api_base: cfg.api_base.trim_end_matches('/').to_string(),
            model: cfg.model.clone(),
            api_key,
            max_variants,
        })
    }

    fn call_expand(&self, query: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Choice {
            message: serde_json::Value,
        }
        #[derive(Deserialize)]
        struct ChatResponse {
            choices: Vec<Choice>,
        }

        let system = format!(
            "You expand search queries into typed variants. \
             Output ONLY lines in this format, no preamble:\n\
             lex: <keyword/BM25 variant>\n\
             vec: <semantic search phrase>\n\
             hyde: <short hypothetical document excerpt>\n\
             Produce at most {} lines total.",
            self.max_variants
        );
        let url = format!("{}/chat/completions", self.api_base);
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&json!({
                "model": self.model,
                "messages": [
                    { "role": "system", "content": system },
                    { "role": "user",   "content": query  }
                ],
                "max_tokens": 256,
                "temperature": 0.7
            }))
            .send()
            .with_context(|| format!("POST {url}"))?
            .error_for_status()
            .with_context(|| format!("chat completions request to {url}"))?;

        let body: ChatResponse = resp.json().context("parsing chat response")?;
        Ok(body
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.get("content")?.as_str().map(str::to_owned))
            .unwrap_or_default())
    }
}

impl Expand for OpenAiExpander {
    fn expand(&self, query: &str) -> Vec<ExpandedQuery> {
        match self.call_expand(query) {
            Ok(raw) => parse_variants(&raw, query, self.max_variants),
            Err(e) => {
                eprintln!("sempkg: OpenAI expander error (falling back to single query): {e}");
                Vec::new()
            }
        }
    }
}

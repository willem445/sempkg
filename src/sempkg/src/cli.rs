/// CLI command definitions using clap.
use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "sempkg",
    version,
    about = "Semantic package manager for sembundle archives",
    long_about = "sempkg manages sembundle semantic index packages, provides scoped \
                  CodeGraph and LanceDB doc queries, and runs an MCP server for AI agents."
)]
pub struct Cli {
    /// Workspace directory (default: current directory)
    #[arg(long, short = 'C', global = true, env = "SEMPKG_WORKSPACE")]
    pub workspace: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    // -----------------------------------------------------------------------
    // Workspace / bundle management
    // -----------------------------------------------------------------------

    /// Initialise a new sempkg.toml in the current (or specified) directory.
    Init {
        /// Registry URL to add to the manifest.
        #[arg(long, short = 'r')]
        registry: Option<String>,
    },

    /// List all registered packages and installed bundles.
    List,

    /// Add a bundle dependency to sempkg.toml.
    ///
    /// Example: sempkg add aws-sdk@1.11.210 --registry https://reg.example.com
    /// Example: sempkg add mylib@2.0.0 --url https://github.com/owner/repo/releases/download/v2.0.0/mylib-2.0.0.sembundle
    Add {
        /// Package spec in `name@version` format.
        spec: String,

        /// Override the registry URL for this dependency.
        #[arg(long, short = 'r')]
        registry_url: Option<String>,

        /// Registry name to use (must match a [[registry]] entry in sempkg.toml).
        #[arg(long)]
        registry: Option<String>,

        /// Direct download URL for the bundle asset (e.g. a GitHub release URL).
        /// When set, no registry is needed.
        #[arg(long, short = 'u')]
        url: Option<String>,

        /// Add to a named dependency group instead of [dependencies].
        #[arg(long, short = 'g')]
        group: Option<String>,
    },

    /// Remove a bundle dependency from sempkg.toml (from [dependencies] or a group).
    Remove {
        /// Package name to remove.
        name: String,

        /// Remove from this named group instead of [dependencies].
        #[arg(long, short = 'g')]
        group: Option<String>,
    },

    /// Install all bundles declared in sempkg.toml.
    Sync {
        /// Re-install even if already present.
        #[arg(long)]
        reinstall: bool,

        /// Also install the named dependency group(s).
        #[arg(long, short = 'g', value_name = "GROUP")]
        group: Vec<String>,

        /// Install every dependency group in addition to [dependencies].
        #[arg(long)]
        all_groups: bool,
    },

    /// Download and install a bundle directly (bypasses manifest).
    Install {
        /// Package spec in `name@version` format.
        spec: String,

        /// Install globally (~/.sempkg/bundles/) instead of workspace-local.
        #[arg(long, short = 'g')]
        global: bool,

        /// Registry URL to fetch from. Required unless --url is provided.
        #[arg(long, short = 'r', required_unless_present = "url")]
        registry: Option<String>,

        /// Direct download URL for the bundle asset (e.g. a GitHub release URL).
        /// When set, --registry is not required.
        #[arg(long, short = 'u')]
        url: Option<String>,

        /// Path to Ed25519 public key PEM file for signature verification.
        #[arg(long)]
        verify_key: Option<PathBuf>,
    },

    /// Show status of an installed bundle or registered package.
    Status {
        /// Package/bundle name.
        name: String,
    },

    /// Repair installed bundles — creates missing .codegraph views so the
    /// codegraph CLI can find bundle indexes. Run this once if you installed
    /// bundles before this fix was applied.
    Repair,

    // -----------------------------------------------------------------------
    // Local package management (for codegraph indexing of local repos)
    // -----------------------------------------------------------------------

    /// Manage locally registered source packages.
    #[command(subcommand)]
    Pkg(PkgCommands),

    // -----------------------------------------------------------------------
    // CodeGraph queries (scoped to one package)
    // -----------------------------------------------------------------------

    /// Search for symbols in a package.
    Search {
        /// Package or bundle name.
        package: String,
        /// Search query.
        query: String,
        /// Filter by symbol kind (function, class, variable, ...).
        #[arg(long, short = 'k')]
        kind: Option<String>,
        /// Max results.
        #[arg(long, short = 'n', default_value = "20")]
        limit: usize,
    },

    /// Find all callers of a symbol.
    Callers {
        /// Package or bundle name.
        package: String,
        /// Symbol name.
        symbol: String,
        #[arg(long, short = 'n', default_value = "20")]
        limit: usize,
    },

    /// Find all callees of a symbol.
    Callees {
        /// Package or bundle name.
        package: String,
        /// Symbol name.
        symbol: String,
        #[arg(long, short = 'n', default_value = "20")]
        limit: usize,
    },

    /// Get AI context for a task description.
    Context {
        /// Package or bundle name.
        package: String,
        /// Task description.
        task: String,
    },

    /// Analyse the downstream impact of changing a symbol.
    Impact {
        /// Package or bundle name.
        package: String,
        /// Symbol name.
        symbol: String,
        /// Call graph traversal depth.
        #[arg(long, short = 'd', default_value = "3")]
        depth: usize,
    },

    /// List files tracked by CodeGraph in a package.
    Files {
        /// Package or bundle name.
        package: String,
        /// Optional path/glob filter.
        #[arg(long, short = 'f')]
        filter: Option<String>,
    },

    // -----------------------------------------------------------------------
    // LanceDB documentation search
    // -----------------------------------------------------------------------

    /// Search the LanceDB documentation index of a bundle.
    Docs {
        /// Bundle name.
        package: String,
        /// Search query.
        query: String,
        /// Max results.
        #[arg(long, short = 'n', default_value = "10")]
        limit: usize,
    },

    // -----------------------------------------------------------------------
    // Hybrid search  (BM25 + reranking)
    // -----------------------------------------------------------------------

    /// Hybrid search: CodeGraph symbols + LanceDB docs + Qwen3-Reranker.
    ///
    /// Fetches candidates from both backends, merges the pool, and scores every
    /// candidate against the query using the local Qwen3-Reranker cross-encoder.
    /// This is the high-quality search path; use `search` or `docs` for the
    /// fast BM25-only path.
    ///
    /// Search modes (matching QMD levels):
    ///   search / docs  →  BM25 full-text search only
    ///   query          →  BM25 (both backends) + Re-ranking
    ///
    /// Requires the binary to be built with `--features reranker` and the model
    /// to be downloaded via `sempkg reranker pull`.
    Query {
        /// Package or bundle name.
        package: String,
        /// Search query.
        query: String,
        /// Restrict query mode to documentation candidates only.
        #[arg(long, conflicts_with = "code")]
        docs: bool,
        /// Restrict query mode to code/symbol candidates only.
        #[arg(long, conflicts_with = "docs")]
        code: bool,
        /// Filter CodeGraph symbol results by kind (function, class, variable, ...).
        #[arg(long, short = 'k')]
        kind: Option<String>,
        /// Number of reranked results to return.
        #[arg(long, short = 'n', default_value = "5")]
        limit: usize,
        /// Override the total hybrid candidate pool size fed into the reranker.
        ///
        /// This is a global budget across combined code + docs candidates
        /// (not per backend). Defaults to `top_k` in [reranker] in
        /// sempkg.toml (20 if unset).
        #[arg(long)]
        top_k: Option<usize>,
    },

    /// Show LanceDB index metadata for a bundle.
    DocsMeta {
        /// Bundle name.
        package: String,
    },

    // -----------------------------------------------------------------------
    // MCP server
    // -----------------------------------------------------------------------

    /// Start the MCP server (JSON-RPC 2.0 over stdio).
    Mcp,

    // -----------------------------------------------------------------------
    // Local LLM reranker management
    // -----------------------------------------------------------------------

    /// Manage the optional Qwen3-Reranker-1.7B GGUF model.
    #[command(subcommand)]
    Reranker(RerankerCommands),
}

#[derive(Subcommand)]
pub enum PkgCommands {
    /// Register a locally cloned repository for CodeGraph indexing.
    Add {
        /// Short identifier (e.g. "pandas").
        name: String,
        /// Absolute or `~`-prefixed path to the repository root.
        path: PathBuf,
        /// Optional one-line description.
        #[arg(long, short = 'd', default_value = "")]
        description: String,
    },

    /// Remove a registered local package (does not delete files or index).
    Remove {
        name: String,
    },

    /// Reindex a registered package to pick up new commits.
    Reindex {
        name: String,
    },

    /// Show codegraph index status for a registered package.
    Status {
        name: String,
    },

    /// List all registered local packages.
    List,

    /// Build or update the LanceDB documentation index for a local package.
    ///
    /// The index is stored at <package-path>/.sempkg/lance/ and requires
    /// no external tools.
    LanceIndex {
        /// Package name (must be registered with `sempkg pkg add`).
        name: String,
        /// Glob pattern of files to index (default: **/*.{md,rst,txt}).
        #[arg(long, default_value = "**/*.{md,rst,txt}")]
        pattern: String,
    },
}

#[derive(Subcommand)]
pub enum RerankerCommands {
    /// Download the Qwen3-Reranker-0.6B GGUF model to ~/.sempkg/models/
    /// (or the path configured in [reranker] in sempkg.toml).
    ///
    /// The default source (ggml-org/Qwen3-Reranker-0.6B-Q8_0-GGUF on HuggingFace)
    /// is public and does not require authentication.
    /// The tokenizer is embedded inside the GGUF — no separate download needed.
    Pull {
        /// Override the GGUF download URL.
        #[arg(long)]
        gguf_url: Option<String>,

        /// HuggingFace access token for downloading gated models.
        /// Can also be supplied via the HF_TOKEN environment variable.
        #[arg(long, env = "HF_TOKEN")]
        hf_token: Option<String>,
    },

    /// Show local model status (present / missing, configured paths, build flags).
    Status,

    /// Score a test query against a document string to verify the model works.
    Test {
        /// The query string.
        query: String,

        /// The document string to score against the query.
        document: String,
    },
}

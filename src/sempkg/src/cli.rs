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

    /// Rebuild and reinstall the local dependency for the current workspace.
    ///
    /// This reuses the settings stored by `sempkg add .`, including
    /// `--include-source`, `--source-glob`, `--source-dir`, `--docs-dir`, and
    /// `--exclude-dir`.
    Refresh,

    /// List all registered packages and installed bundles.
    List,

    /// Add a bundle dependency to sempkg.toml.
    ///
    /// Example: sempkg add aws-sdk@1.11.210 --registry https://reg.example.com
    /// Example: sempkg add mylib@2.0.0 --url https://github.com/owner/repo/releases/download/v2.0.0/mylib-2.0.0.sembundle
    /// Example: sempkg add --url https://github.com/pandas-dev/pandas/releases/tag/v3.0.3 --full --include-source
    /// Example: sempkg add pandas-dev/pandas@v2.2.2
    /// Example: sempkg add https://github.com/pandas-dev/pandas/tree/v2.2.2
    /// Example: sempkg add . --name sempkg --include-source --docs-dir docs --source-dir src/sempkg/src
    /// Example: sempkg add /path/to/sdk --name my-sdk
    /// Example: sempkg add ~/tools/llvm --name llvm --version 17.0
    /// Example: sempkg add C:\LLVM --name llvm
    ///
    /// When a GitHub source is provided, sempkg immediately fetches, builds,
    /// and installs the bundle into the workspace (no separate `sync` needed).
    ///
    /// When a local filesystem path is provided (including `.` for the current
    /// directory), sempkg builds the bundle directly from that directory and
    /// installs it.  The path is recorded in `sempkg.toml` so `sempkg sync`
    /// and `sempkg refresh` can rebuild it later.
    Add {
        /// Package spec in `name@version` format, GitHub shorthand `owner/repo@ref`,
        /// or a full GitHub URL.
        ///
        /// May be omitted when `--url` is used with a GitHub source URL. Direct
        /// bundle asset URLs still require a separate `name@version` spec.
        #[arg(required_unless_present = "url")]
        spec: Option<String>,

        /// Override the registry URL for this dependency.
        #[arg(long, short = 'r')]
        registry_url: Option<String>,

        /// Registry name to use (must match a [[registry]] entry in sempkg.toml).
        #[arg(long)]
        registry: Option<String>,

        /// Direct download URL for the bundle asset.
        ///
        /// A GitHub source URL can also be supplied here as an alternative to the
        /// positional spec, for example with `--full` / `--include-source`.
        /// When set, no registry is needed.
        #[arg(long, short = 'u')]
        url: Option<String>,

        /// Add to a named dependency group instead of [dependencies].
        #[arg(long, short = 'g')]
        group: Option<String>,

        /// Force the full build path even when a release `.sembundle` asset exists.
        #[arg(long)]
        build: bool,

        /// Re-build and reinstall even if this bundle version is already installed.
        #[arg(long)]
        reinstall: bool,

        /// Perform a shallow git clone instead of downloading a tar.gz archive.
        ///
        /// Use this when the repo's GitHub archive is export-filtered and omits docs
        /// (like pandas, CPython, etc.). Requires `git` on PATH. The clone is
        /// single-branch and shallow (--depth 1) so it is still fast.
        #[arg(long)]
        full: bool,

        /// Override the package name derived from the repo name.
        #[arg(long)]
        name: Option<String>,

        /// Override the version derived from the git ref.
        #[arg(long, short = 'v')]
        version: Option<String>,

        /// Build a LanceDB source-code index (chunked by top-level symbols) and
        /// embed it in the bundle.  Enables the `search_code` and `read_symbol`
        /// MCP tools and augments `get_callers`/`get_callees` with source bodies.
        #[arg(long)]
        include_source: bool,

        /// Glob mask restricting which source files are included in the code index
        /// (only meaningful with --include-source).
        /// Default covers Rust, Python, JS/TS, Go, Java, C/C++.
        #[arg(long)]
        source_glob: Option<String>,

        /// Source directory to index with codegraph. Repeat the flag to index
        /// multiple directories. Defaults to the whole source root.
        #[arg(long = "source-dir", short = 's')]
        source_dirs: Vec<PathBuf>,

        /// Documentation directory to index with LanceDB. Repeat the flag to
        /// add multiple directories. Defaults to the whole source root.
        #[arg(long = "docs-dir", short = 'd')]
        docs_dirs: Vec<PathBuf>,

        /// Directory to exclude from all indexing (source, docs, and source-code
        /// index). Repeat the flag to exclude multiple directories.
        #[arg(long = "exclude-dir", short = 'e')]
        exclude_dirs: Vec<PathBuf>,

        /// Optional one-line description of what this bundle provides. Shown in
        /// `sempkg list` and the MCP `list_packages` tool so agents know which
        /// package to search. Preserved across `sempkg sync` / `refresh`.
        #[arg(long)]
        description: Option<String>,
    },

    /// Remove a workspace dependency or global package state.
    ///
    /// By default, this removes the dependency from sempkg.toml and deletes the
    /// package from the workspace store. Use --global to delete matching global
    /// package registrations and global bundle installs without modifying the
    /// workspace manifest.
    Remove {
        /// Package name to remove.
        name: String,

        /// Remove from this named group instead of [dependencies].
        ///
        /// Only valid for workspace removals.
        #[arg(long, short = 'g')]
        group: Option<String>,

        /// Remove from global package state without touching sempkg.toml.
        #[arg(long, short = 'G')]
        global: bool,
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

    /// Show a diagnostic report for this sempkg installation, or the status of
    /// one installed bundle / registered package.
    ///
    /// With no NAME, prints version, build features, GPU backend, model, store,
    /// and CodeGraph diagnostics — the information a bug report needs.
    /// With a NAME, prints the status of that bundle or registered package.
    Status {
        /// Package/bundle name. Omit for the installation-wide diagnostic report.
        name: Option<String>,

        /// Print the diagnostic report as JSON. Only valid without a NAME.
        #[arg(long, conflicts_with = "name")]
        json: bool,
    },

    /// Uninstall a bundle (remove from local or global store).
    ///
    /// Does not modify sempkg.toml or .lock files. Use `sempkg remove` to remove
    /// from the manifest, or manually edit sempkg.toml and run `sempkg sync` to
    /// reinstall dependencies.
    Uninstall {
        /// Package spec in `name@version` format.
        spec: String,

        /// Uninstall from global store (~/.sempkg/bundles/) instead of workspace-local.
        #[arg(long, short = 'g')]
        global: bool,
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
        /// Optional glob pattern (e.g. **/*.rs) or substring (e.g. auth).
        #[arg(long, short = 'f')]
        filter: Option<String>,
        /// Max files to return (default 200).
        #[arg(long, default_value_t = 200)]
        limit: usize,
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

    // -----------------------------------------------------------------------
    // Vector embedding management
    // -----------------------------------------------------------------------
    /// Generate vector embeddings for installed bundles / local packages.
    ///
    /// Embeds the `docs` and `code` LanceDB tables in place so the MCP `query`
    /// tool can run semantic (vector) search alongside BM25. Requires the binary
    /// to be built with `--features embeddings` and the model downloaded via
    /// `sempkg embedding pull`.
    Embed {
        /// Restrict embedding to a single package / bundle (default: all).
        package: Option<String>,
        /// Re-embed even if the table already has vectors for this model.
        #[arg(long)]
        force: bool,
    },

    /// Manage the optional Qwen3-Embedding-0.6B GGUF model (vector search).
    #[command(subcommand)]
    Embedding(EmbeddingCommands),

    /// Manage the optional query-expansion GGUF model.
    #[command(subcommand, name = "query-expansion")]
    QueryExpansion(QueryExpansionCommands),
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
    Remove { name: String },

    /// Reindex a registered package to pick up new commits.
    Reindex { name: String },

    /// Show codegraph index status for a registered package.
    Status { name: String },

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

#[derive(Subcommand)]
pub enum EmbeddingCommands {
    /// Download an embedding GGUF model to ~/.sempkg/models/.
    ///
    /// With no `--model`, pulls the model configured in [embedding] in
    /// sempkg.toml (default: embeddinggemma-300m). Both default sources are
    /// public GGUF repos on HuggingFace and do not require authentication.
    Pull {
        /// Which model to download: `embeddinggemma-300m` (default) or
        /// `qwen3-embedding-0.6b`. Defaults to the configured model.
        #[arg(long)]
        model: Option<String>,

        /// Override the GGUF download URL.
        #[arg(long)]
        gguf_url: Option<String>,
    },

    /// Show embedding model status (present / missing, configured paths, build flags).
    Status,
}

#[derive(Subcommand)]
pub enum QueryExpansionCommands {
    /// Download the query-expansion GGUF model to ~/.sempkg/models/
    /// (or the path configured in [query_expansion] in sempkg.toml).
    ///
    /// The default source (tobil/qmd-query-expansion-1.7B-gguf on HuggingFace)
    /// is public and does not require authentication.
    Pull {
        /// Override the GGUF download URL.
        #[arg(long)]
        gguf_url: Option<String>,
    },

    /// Show query-expansion model status.
    Status,

    /// Expand a test query and print the routed variants.
    Test {
        /// The query string to expand.
        query: String,
    },
}

#[cfg(test)]
mod tests {
    use super::{Cli, Commands};
    use clap::{CommandFactory, Parser};
    use std::path::PathBuf;

    /// Model downloads are anonymous-only (#106): sempkg does not want to carry the
    /// risk of handling a user's HuggingFace credential. No `pull` command may take
    /// a token — not as a flag, and not from the environment. This pins the contract
    /// against a well-meaning re-introduction.
    ///
    /// Asserted against clap's own arg definitions rather than by setting an env
    /// var, because `set_var` races with the other tests parsing in parallel.
    #[test]
    fn pull_commands_accept_no_token_at_all() {
        let cli = Cli::command();

        for (group, sub) in [
            ("reranker", "pull"),
            ("embedding", "pull"),
            ("query-expansion", "pull"),
        ] {
            let pull = cli
                .find_subcommand(group)
                .unwrap_or_else(|| panic!("`{group}` subcommand"))
                .find_subcommand(sub)
                .unwrap_or_else(|| panic!("`{group} {sub}` subcommand"));

            for arg in pull.get_arguments() {
                let id = arg.get_id().as_str();
                assert!(
                    !id.contains("token"),
                    "`{group} {sub}` must take no token argument, found `--{id}`"
                );
                assert_eq!(
                    arg.get_env(),
                    None,
                    "`{group} {sub} --{id}` must not read a value from the environment"
                );
            }
        }
    }

    #[test]
    fn add_accepts_github_source_url_via_flag_without_spec() {
        let cli = Cli::try_parse_from([
            "sempkg",
            "add",
            "--url",
            "https://github.com/pandas-dev/pandas/releases/tag/v3.0.3",
            "--full",
            "--include-source",
        ])
        .expect("add command should parse");

        match cli.command {
            Commands::Add {
                spec,
                url,
                full,
                include_source,
                ..
            } => {
                assert_eq!(spec, None);
                assert_eq!(
                    url.as_deref(),
                    Some("https://github.com/pandas-dev/pandas/releases/tag/v3.0.3")
                );
                assert!(full);
                assert!(include_source);
            }
            _ => panic!("expected add command"),
        }
    }

    #[test]
    fn add_accepts_current_directory_shortcut() {
        let cli = Cli::try_parse_from([
            "sempkg",
            "add",
            ".",
            "--include-source",
            "--docs-dir",
            "docs",
        ])
        .expect("add command should parse");

        match cli.command {
            Commands::Add {
                spec,
                include_source,
                docs_dirs,
                ..
            } => {
                assert_eq!(spec.as_deref(), Some("."));
                assert!(include_source);
                assert_eq!(docs_dirs, vec![PathBuf::from("docs")]);
            }
            _ => panic!("expected add command"),
        }
    }

    #[test]
    fn add_accepts_description_flag() {
        let cli = Cli::try_parse_from([
            "sempkg",
            "add",
            "owner/repo@v1.0",
            "--description",
            "CAN bus utilities",
        ])
        .expect("add command should parse");

        match cli.command {
            Commands::Add {
                spec, description, ..
            } => {
                assert_eq!(spec.as_deref(), Some("owner/repo@v1.0"));
                assert_eq!(description.as_deref(), Some("CAN bus utilities"));
            }
            _ => panic!("expected add command"),
        }
    }

    #[test]
    fn refresh_command_parses() {
        let cli = Cli::try_parse_from(["sempkg", "refresh"]).expect("refresh should parse");

        assert!(matches!(cli.command, Commands::Refresh));
    }

    /// Bare `sempkg status` is the diagnostic report. On the old CLI (NAME
    /// required) clap rejected it outright, so this is the parse that had to
    /// start working.
    #[test]
    fn status_without_name_parses() {
        let cli = Cli::try_parse_from(["sempkg", "status"])
            .expect("bare `sempkg status` should parse as the diagnostic report");

        assert!(matches!(cli.command, Commands::Status { .. }));
    }

    #[test]
    fn status_without_name_has_no_package_and_no_json() {
        let cli = Cli::try_parse_from(["sempkg", "status"]).expect("bare status should parse");

        match cli.command {
            Commands::Status { name, json } => {
                assert_eq!(name, None);
                assert!(!json);
            }
            _ => panic!("expected status command"),
        }
    }

    #[test]
    fn status_json_flag_parses() {
        let cli = Cli::try_parse_from(["sempkg", "status", "--json"])
            .expect("`sempkg status --json` should parse");

        assert!(matches!(cli.command, Commands::Status { .. }));
    }

    /// Regression pin: `sempkg status <name>` keeps its existing meaning — the
    /// name is still accepted positionally and reaches the same code path.
    #[test]
    fn status_with_name_parses() {
        let cli = Cli::try_parse_from(["sempkg", "status", "aws-sdk"])
            .expect("`sempkg status <name>` should keep parsing");

        match cli.command {
            Commands::Status { name, json } => {
                assert_eq!(name.as_deref(), Some("aws-sdk"));
                assert!(!json);
            }
            _ => panic!("expected status command"),
        }
    }

    /// `--json` only renders the installation report; asking for JSON *and* a
    /// package name is rejected rather than silently ignoring one of them.
    #[test]
    fn status_name_with_json_is_rejected() {
        let err = Cli::try_parse_from(["sempkg", "status", "aws-sdk", "--json"])
            .err()
            .expect("`status <name> --json` should be rejected");

        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }
}

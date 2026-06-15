mod build;
mod checksum;
mod error;
mod manifest;
mod pack;
mod validate;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use build::BuildOptions;
use pack::PackOptions;

#[derive(Parser)]
#[command(
    name = "cgbundle",
    about = "Pack CodeGraph output directories into portable .cgbundle archives",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Pack a CodeGraph output directory into a .cgbundle archive
    Pack {
        /// Path to the CodeGraph output directory.
        /// Must contain: graph/, embeddings/, config.json
        input_dir: PathBuf,

        /// Package name (lowercase letters, digits, and hyphens; min 2 chars)
        #[arg(long, short = 'n')]
        name: String,

        /// Package version string (e.g. 1.2.3 or humble)
        #[arg(long, short = 'r')]
        version: String,

        /// Canonical source repository URL
        #[arg(long)]
        source_repo: String,

        /// Full 40-character lowercase Git commit SHA
        #[arg(long)]
        commit_hash: String,

        /// Version of CodeGraph used to produce the index
        #[arg(long)]
        codegraph_version: String,

        /// Git tag associated with this release (optional)
        #[arg(long)]
        tag: Option<String>,

        /// Primary language indexed (e.g. python, cpp, rust)
        #[arg(long, default_value = "unknown")]
        language: String,

        /// Repo-relative paths that were indexed, comma-separated
        /// (default: ".")
        #[arg(long, value_delimiter = ',')]
        indexed_paths: Vec<String>,

        /// Output .cgbundle file path
        /// (default: ./<name>-<version>.cgbundle)
        #[arg(long, short = 'o')]
        output: Option<PathBuf>,

        /// Path to a project-local QMD index directory to include as the `qmd/` extension.
        ///
        /// The directory must contain:
        ///   index/index.sqlite  — QMD SQLite database
        ///   embeddings/         — non-empty vector export
        ///   metadata.json       — QMD indexing metadata
        ///   config.json         — QMD collection configuration
        /// model.gguf is optional. When supplied, sets extensions=["qmd"] in manifest.json.
        #[arg(long)]
        qmd_dir: Option<PathBuf>,
    },

    /// Index source and docs directories, then pack everything into a .cgbundle
    Build {
        // --- Bundle identity ---
        /// Package name (lowercase letters, digits, and hyphens; min 2 chars)
        #[arg(long, short = 'n')]
        name: String,

        /// Package version string (e.g. 1.2.3)
        #[arg(long, short = 'r')]
        version: String,

        /// Canonical source repository URL
        #[arg(long)]
        source_repo: String,

        /// Full 40-character lowercase Git commit SHA
        #[arg(long)]
        commit_hash: String,

        /// Version of CodeGraph used to produce the index
        #[arg(long)]
        codegraph_version: String,

        /// Git tag associated with this release (optional)
        #[arg(long)]
        tag: Option<String>,

        /// Primary language indexed (e.g. python, cpp, rust)
        #[arg(long, default_value = "unknown")]
        language: String,

        /// Output .cgbundle file path (default: ./<name>-<version>.cgbundle)
        #[arg(long, short = 'o')]
        output: Option<PathBuf>,

        // --- CodeGraph inputs ---
        /// Source directory to index with `codegraph init --index`.
        /// Repeat the flag to index multiple directories. At least one required.
        #[arg(long = "source-dir", short = 's', required = true)]
        source_dirs: Vec<PathBuf>,

        // --- QMD inputs (optional) ---
        /// Documentation directory to index with QMD.
        /// Repeat the flag to add multiple directories.
        #[arg(long = "docs-dir", short = 'd')]
        docs_dirs: Vec<PathBuf>,

        /// QMD collection name (default: bundle name)
        #[arg(long)]
        qmd_collection_name: Option<String>,

        /// Glob mask for QMD document discovery (default: **/*.{md,txt,rst})
        #[arg(long)]
        qmd_glob: Option<String>,

        /// QMD chunking strategy: "auto" (AST-aware) or "regex" (default: auto)
        #[arg(long, default_value = "auto")]
        qmd_chunk_strategy: String,
    },
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Build {
            name,
            version,
            source_repo,
            commit_hash,
            codegraph_version,
            tag,
            language,
            output,
            source_dirs,
            docs_dirs,
            qmd_collection_name,
            qmd_glob,
            qmd_chunk_strategy,
        } => build::build(BuildOptions {
            name,
            version,
            source_repo,
            commit_hash,
            tag,
            language,
            codegraph_version,
            output_path: output,
            source_dirs,
            docs_dirs,
            qmd_collection_name,
            qmd_glob,
            qmd_chunk_strategy,
        })
        .map(|path| {
            println!("Bundle created: {}", path.display());
        }),

        Commands::Pack {
            input_dir,
            name,
            version,
            source_repo,
            commit_hash,
            codegraph_version,
            tag,
            language,
            indexed_paths,
            output,
            qmd_dir,
        } => pack::pack(PackOptions {
            input_dir,
            output_path: output,
            name,
            version,
            source_repo,
            commit_hash,
            tag,
            language,
            indexed_paths,
            codegraph_version,
            qmd_dir,
        })
        .map(|path| {
            println!("Bundle created: {}", path.display());
        }),
    };

    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

mod checksum;
mod error;
mod manifest;
mod pack;
mod validate;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

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
    },
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
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

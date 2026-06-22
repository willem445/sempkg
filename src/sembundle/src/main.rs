mod build;
mod checksum;
mod error;
mod keygen;
mod manifest;
mod pack;
mod publish;
mod sign;
mod validate;
mod verify;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use build::BuildOptions;
use keygen::KeygenOptions;
use pack::PackOptions;
use sign::SignOptions;
use verify::VerifyOptions;

#[derive(Parser)]
#[command(
    name = "sembundle",
    about = "Pack CodeGraph output directories into portable .sembundle archives",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Pack a CodeGraph output directory into a .sembundle archive
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

        /// Output .sembundle file path
        /// (default: ./<name>-<version>.sembundle)
        #[arg(long, short = 'o')]
        output: Option<PathBuf>,

        /// Path to a pre-built LanceDB directory to include as the `lance/` extension.
        ///
        /// The directory must contain:
        ///   metadata.json  — LanceDB indexing metadata
        ///   docs.lance/    — LanceDB Arrow table directory
        /// When supplied, sets extensions=["lance"] in manifest.json.
        #[arg(long)]
        lance_dir: Option<PathBuf>,

        /// Path to a pre-built LanceDB source-code index directory to include as
        /// the `code/` extension.
        ///
        /// The directory must contain:
        ///   metadata.json  — code index metadata
        ///   code.lance/    — LanceDB Arrow table directory
        #[arg(long)]
        code_dir: Option<PathBuf>,
    },

    /// Publish a .sembundle to a registry server
    Publish {
        /// Path to the .sembundle file to publish
        bundle_path: PathBuf,

        /// Registry server base URL (e.g. http://192.168.1.10:8765).
        /// Can also be set via SemBundle_REGISTRY_URL env var.
        #[arg(long, env = "SemBundle_REGISTRY_URL")]
        registry: Option<String>,

        /// Publish token for authentication.
        /// Can also be set via SemBundle_TOKEN env var.
        #[arg(long, env = "SemBundle_TOKEN")]
        token: Option<String>,
    },

    /// Index source and docs directories, then pack everything into a .sembundle
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

        /// Output .sembundle file path (default: ./<name>-<version>.sembundle)
        #[arg(long, short = 'o')]
        output: Option<PathBuf>,

        // --- CodeGraph inputs ---
        /// Source directory to index with `codegraph init --index`.
        /// Repeat the flag to index multiple directories. At least one required.
        #[arg(long = "source-dir", short = 's', required = true)]
        source_dirs: Vec<PathBuf>,

        // --- Lance inputs (optional) ---
        /// Documentation directory to index with LanceDB.
        /// Repeat the flag to add multiple directories.
        #[arg(long = "docs-dir", short = 'd')]
        docs_dirs: Vec<PathBuf>,

        /// Glob mask for document discovery (default: **/*.md,**/*.txt,**/*.rst)
        #[arg(long)]
        docs_glob: Option<String>,

        // --- Source-code index (optional) ---
        /// Build a LanceDB source-code index chunked by top-level symbols and
        /// embed it in the bundle as the `code/` extension.
        #[arg(long, default_value_t = false)]
        include_source: bool,

        /// Glob mask restricting which files are included in the source-code index.
        /// Default covers common compiled and scripted languages.
        #[arg(long)]
        source_glob: Option<String>,

        // --- Exclusions ---
        /// Directory to exclude from all indexing (source, docs, and source-code index).
        /// Repeat the flag to exclude multiple directories.
        /// Absolute paths are matched directly; relative paths are matched against
        /// each entry's path relative to its base directory.
        #[arg(long = "exclude-dir", short = 'x')]
        exclude_dirs: Vec<PathBuf>,
    },

    /// Generate an Ed25519 keypair for bundle signing
    KeyGen {
        /// Output directory for private.pem and public.pem (default: current directory)
        #[arg(long, short = 'o', default_value = ".")]
        output_dir: PathBuf,
    },

    /// Sign a .sembundle file with an Ed25519 private key
    Sign {
        /// Path to the .sembundle file to sign
        bundle_path: PathBuf,
        /// Path to the Ed25519 private key PEM file
        #[arg(long, short = 'k')]
        key: PathBuf,
        /// Output .sig file path (default: <bundle_path>.sig)
        #[arg(long, short = 'o')]
        output: Option<PathBuf>,
    },

    /// Verify a .sembundle signature
    Verify {
        /// Path to the .sembundle file
        bundle_path: PathBuf,
        /// Path to the .sig file
        #[arg(long, short = 's')]
        sig: PathBuf,
        /// Path to the Ed25519 public key PEM file
        #[arg(long, short = 'k')]
        key: PathBuf,
    },
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Publish {
            bundle_path,
            registry,
            token,
        } => {
            let registry_url = match registry {
                Some(u) => u,
                None => {
                    eprintln!("error: {}", publish::PublishError::MissingRegistry);
                    std::process::exit(1);
                }
            };
            let token = match token {
                Some(t) => t,
                None => {
                    eprintln!("error: {}", publish::PublishError::MissingToken);
                    std::process::exit(1);
                }
            };
            publish::publish(publish::PublishOptions {
                bundle_path,
                registry_url: registry_url.clone(),
                token,
            })
            .map(|(name, version)| {
                println!("Published {name}@{version} to {registry_url}");
            })
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
        }

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
            docs_glob,
            include_source,
            source_glob,
            exclude_dirs,
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
            docs_glob,
            include_source,
            source_glob,
            exclude_dirs,
        })
        .map(|path| {
            println!("Bundle created: {}", path.display());
        })
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error>),

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
            lance_dir,
            code_dir,
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
            lance_dir,
            code_dir,
        })
        .map(|path| {
            println!("Bundle created: {}", path.display());
        })
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error>),

        Commands::KeyGen { output_dir } => {
            keygen::keygen(KeygenOptions { output_dir })
                .map(|(private_path, public_path)| {
                    println!("Private key: {}", private_path.display());
                    println!("Public key: {}", public_path.display());
                })
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
        }

        Commands::Sign {
            bundle_path,
            key,
            output,
        } => sign::sign(SignOptions {
            bundle_path,
            private_key_path: key,
            output,
        })
        .map(|path| {
            println!("Signature written: {}", path.display());
        })
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error>),

        Commands::Verify {
            bundle_path,
            sig,
            key,
        } => verify::verify(VerifyOptions {
            bundle_path,
            sig_path: sig,
            public_key_path: key,
        })
        .map(|()| {
            println!("Signature valid.");
        })
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error>),
    };

    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

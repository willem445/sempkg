//! Build pipeline: run codegraph and QMD against source / docs directories,
//! then pack the results into a `.cgbundle` archive.
//!
//! This is the implementation behind the `cgbundle build` subcommand.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::json;
use walkdir::WalkDir;

use crate::error::PackError;
use crate::manifest::QmdMetadata;
use crate::pack::{pack, PackOptions};
use crate::validate::validate_name;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Options for the `build` subcommand.
pub struct BuildOptions {
    // --- Bundle identity (mirrors PackOptions) ---
    pub name: String,
    pub version: String,
    pub source_repo: String,
    pub commit_hash: String,
    pub tag: Option<String>,
    pub language: String,
    pub codegraph_version: String,
    /// Where to write the finished `.cgbundle`. Defaults to `./<name>-<version>.cgbundle`.
    pub output_path: Option<PathBuf>,

    // --- CodeGraph inputs ---
    /// Source directories to index with `codegraph init --index`.
    /// At least one is required.
    pub source_dirs: Vec<PathBuf>,

    // --- QMD inputs (optional) ---
    /// Documentation directories to index with QMD. Empty = no QMD extension.
    pub docs_dirs: Vec<PathBuf>,
    /// QMD collection name. Defaults to the bundle `name`.
    pub qmd_collection_name: Option<String>,
    /// Glob mask for QMD document discovery. Default: `**/*.{md,txt,rst}`.
    pub qmd_glob: Option<String>,
    /// QMD chunking strategy: `"auto"` (AST-aware) or `"regex"`.
    pub qmd_chunk_strategy: String,
}

/// Run the full build pipeline and return the path of the produced bundle.
pub fn build(opts: BuildOptions) -> Result<PathBuf, PackError> {
    validate_name(&opts.name)?;

    if opts.source_dirs.is_empty() {
        return Err(PackError::InvalidField {
            field: "source_dirs".to_string(),
            reason: "at least one --source-dir is required".to_string(),
        });
    }

    if !matches!(opts.qmd_chunk_strategy.as_str(), "auto" | "regex") {
        return Err(PackError::InvalidField {
            field: "qmd_chunk_strategy".to_string(),
            reason: "must be 'auto' or 'regex'".to_string(),
        });
    }

    // Temporary working directory. Dropped (deleted) after pack() succeeds.
    let work = tempfile::TempDir::new()?;
    let cg_out = work.path().join("codegraph-out");
    std::fs::create_dir_all(&cg_out)?;

    // Step 1: index source directories with codegraph.
    eprintln!("[cgbundle] Running codegraph ...");
    run_codegraph(&opts.source_dirs, &cg_out)?;

    // Step 2: index docs directories with QMD (optional).
    let qmd_out = if !opts.docs_dirs.is_empty() {
        let qmd_dir = work.path().join("qmd-out");
        let collection = opts.qmd_collection_name.as_deref().unwrap_or(&opts.name);
        let glob = opts.qmd_glob.as_deref().unwrap_or("**/*.{md,txt,rst}");
        eprintln!("[cgbundle] Running QMD ...");
        run_qmd(
            &opts.docs_dirs,
            work.path(),
            &qmd_dir,
            collection,
            glob,
            &opts.qmd_chunk_strategy,
        )?;
        Some(qmd_dir)
    } else {
        None
    };

    // Step 3: pack.
    eprintln!("[cgbundle] Packing bundle ...");
    let bundle_path = pack(PackOptions {
        input_dir: cg_out,
        output_path: opts.output_path,
        name: opts.name,
        version: opts.version,
        source_repo: opts.source_repo,
        commit_hash: opts.commit_hash,
        tag: opts.tag,
        language: opts.language,
        indexed_paths: opts
            .source_dirs
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        codegraph_version: opts.codegraph_version,
        qmd_dir: qmd_out,
    })?;

    Ok(bundle_path)
    // `work` is dropped here, cleaning up all intermediate files.
}

// ---------------------------------------------------------------------------
// CodeGraph step
// ---------------------------------------------------------------------------

fn run_codegraph(source_dirs: &[PathBuf], out_dir: &Path) -> Result<(), PackError> {
    let exe = find_tool("codegraph")?;
    let graph_dir = out_dir.join("graph");
    std::fs::create_dir_all(&graph_dir)?;

    for source_dir in source_dirs {
        eprintln!(
            "[cgbundle]   codegraph: indexing {} ...",
            source_dir.display()
        );
        // codegraph stores its output under <source_dir>/.codegraph/
        invoke(
            &exe,
            &["init", "--index", &source_dir.to_string_lossy()],
            None,
            None,
            true,
        )?;

        let dot_cg = source_dir.join(".codegraph");
        if dot_cg.is_dir() {
            copy_dir_into(&dot_cg, &graph_dir)?;
        }
    }

    // embeddings/: record which source directories were indexed.
    // Actual semantic embedding vectors are implementation-defined per the spec
    // and will be produced by future CodeGraph export support.
    let emb_dir = out_dir.join("embeddings");
    std::fs::create_dir_all(&emb_dir)?;
    std::fs::write(
        emb_dir.join("source-index.json"),
        serde_json::to_vec_pretty(&json!({
            "format": "codegraph-source-index",
            "source_dirs": source_dirs
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
        }))?,
    )?;

    // config.json: prefer codegraph's own config if it wrote one.
    let config_dst = out_dir.join("config.json");
    if !config_dst.exists() {
        let cg_config = source_dirs
            .first()
            .map(|d| d.join(".codegraph").join("config.json"))
            .filter(|p| p.is_file());
        if let Some(src) = cg_config {
            std::fs::copy(&src, &config_dst)?;
        } else {
            std::fs::write(&config_dst, b"{}")?;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// QMD step
// ---------------------------------------------------------------------------

fn run_qmd(
    docs_dirs: &[PathBuf],
    work_dir: &Path,
    out_dir: &Path,
    collection: &str,
    glob: &str,
    chunk_strategy: &str,
) -> Result<(), PackError> {
    let exe = find_tool("qmd")?;

    // Redirect QMD's cache to an isolated directory so the build never
    // reads from or writes to the user's global ~/.cache/qmd/ store.
    let qmd_cache = work_dir.join("qmd-cache");
    std::fs::create_dir_all(&qmd_cache)?;

    // Add one collection per docs directory.
    for (i, docs_dir) in docs_dirs.iter().enumerate() {
        let coll_name = if i == 0 {
            collection.to_string()
        } else {
            format!("{collection}-{}", i + 1)
        };
        eprintln!(
            "[cgbundle]   qmd: adding collection '{}' from {} ...",
            coll_name,
            docs_dir.display()
        );
        invoke(
            &exe,
            &[
                "collection",
                "add",
                &docs_dir.to_string_lossy(),
                "--name",
                &coll_name,
                "--mask",
                glob,
            ],
            None,
            Some(("XDG_CACHE_HOME", &qmd_cache)),
            false,
        )?;
    }

    eprintln!("[cgbundle]   qmd: updating index ...");
    invoke(
        &exe,
        &["update"],
        None,
        Some(("XDG_CACHE_HOME", &qmd_cache)),
        false,
    )?;

    eprintln!("[cgbundle]   qmd: generating embeddings (this may take a while on first run) ...");
    invoke(
        &exe,
        &["embed", "--chunk-strategy", chunk_strategy],
        None,
        Some(("XDG_CACHE_HOME", &qmd_cache)),
        true, // pass through: lets the user see model download progress
    )?;

    // Locate the SQLite database that QMD wrote.
    let db_src = qmd_cache.join("qmd").join("index.sqlite");
    if !db_src.is_file() {
        return Err(PackError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "QMD index not found at '{}'; ensure QMD ran successfully",
                db_src.display()
            ),
        )));
    }

    // Copy the database to the canonical bundle path.
    let index_dir = out_dir.join("index");
    std::fs::create_dir_all(&index_dir)?;
    std::fs::copy(&db_src, index_dir.join("index.sqlite"))?;

    // Generate qmd/embeddings/ and qmd/metadata.json.
    let qmd_version = get_version(&exe);
    generate_qmd_artifacts(out_dir, collection, docs_dirs, glob, chunk_strategy, &qmd_version)?;

    Ok(())
}

fn generate_qmd_artifacts(
    out_dir: &Path,
    collection: &str,
    docs_dirs: &[PathBuf],
    glob: &str,
    chunk_strategy: &str,
    qmd_version: &str,
) -> Result<(), PackError> {
    // qmd/embeddings/: reference file pointing consumers to the sqlite database.
    // Actual float32 vectors are stored via sqlite-vec inside index.sqlite.
    // Format identifier "qmd-sqlite-reference" is implementation-defined per spec §9.3.
    let emb_dir = out_dir.join("embeddings");
    std::fs::create_dir_all(&emb_dir)?;
    std::fs::write(
        emb_dir.join("index.json"),
        serde_json::to_vec_pretty(&json!({
            "format": "qmd-sqlite-reference",
            "db": "qmd/index/index.sqlite",
            "note": "Embedding vectors are stored in the QMD SQLite database (sqlite-vec format). \
                     Use the QMD SDK or CLI to query them: createStore({ dbPath: 'qmd/index/index.sqlite' })"
        }))?,
    )?;

    // Resolve embed model: respect QMD_EMBED_MODEL env override.
    let embed_model = std::env::var("QMD_EMBED_MODEL").unwrap_or_else(|_| {
        "hf:ggml-org/embeddinggemma-300M-GGUF/embeddinggemma-300M-Q8_0.gguf".to_string()
    });
    // Dimension heuristic: 1024 for Qwen3-Embedding family, 768 otherwise.
    let embedding_dim: u64 = if embed_model.contains("Qwen3-Embedding") {
        1024
    } else {
        768
    };

    // qmd/metadata.json (document_count / chunk_count left at 0 — populated
    // in a future release once rusqlite is added as a dependency).
    let metadata = QmdMetadata {
        qmd_version: qmd_version.to_string(),
        embed_model,
        embed_model_hash: None,
        chunk_strategy: chunk_strategy.to_string(),
        embeddings_format: "qmd-sqlite-reference".to_string(),
        embedding_dim,
        collection_name: collection.to_string(),
        document_count: 0,
        chunk_count: 0,
        indexed_paths: docs_dirs
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        created_at: String::new(), // stamped by pack()
    };
    std::fs::write(
        out_dir.join("metadata.json"),
        serde_json::to_vec_pretty(&metadata)?,
    )?;

    // qmd/config.json: record collection → path mapping.
    let colls: serde_json::Map<String, serde_json::Value> = docs_dirs
        .iter()
        .enumerate()
        .map(|(i, dir)| {
            let name = if i == 0 {
                collection.to_string()
            } else {
                format!("{collection}-{}", i + 1)
            };
            (
                name,
                json!({ "path": dir.to_string_lossy(), "pattern": glob }),
            )
        })
        .collect();
    std::fs::write(
        out_dir.join("config.json"),
        serde_json::to_vec_pretty(&json!({ "collections": colls }))?,
    )?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Tool helpers
// ---------------------------------------------------------------------------

/// Resolve an executable on PATH.  On Windows, `which` finds `.cmd` wrappers.
fn find_tool(name: &str) -> Result<PathBuf, PackError> {
    which::which(name).map_err(|_| PackError::ToolNotFound(name.to_string()))
}

/// Get the version string from a tool by running `<exe> --version`.
fn get_version(exe: &Path) -> String {
    build_command(exe, &["--version"])
        .output()
        .ok()
        .and_then(|o| {
            let stdout = String::from_utf8_lossy(&o.stdout).trim().to_string();
            let out = if stdout.is_empty() {
                String::from_utf8_lossy(&o.stderr).trim().to_string()
            } else {
                stdout
            };
            // Take the first word and strip a leading 'v'.
            out.split_whitespace()
                .next()
                .map(|s| s.trim_start_matches('v').to_string())
        })
        .unwrap_or_else(|| "unknown".to_string())
}

/// Build a `Command` for `exe`, wrapping `.cmd`/`.bat` files with `cmd /C` on Windows.
fn build_command(exe: &Path, args: &[&str]) -> Command {
    #[cfg(windows)]
    {
        let ext = exe
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if ext == "cmd" || ext == "bat" {
            let mut cmd = Command::new("cmd");
            cmd.arg("/C").arg(exe).args(args);
            return cmd;
        }
    }
    let mut cmd = Command::new(exe);
    cmd.args(args);
    cmd
}

/// Run `exe args`, optionally setting one env var and choosing pass-through
/// vs. captured-output mode.
///
/// * `passthrough = true`  — inherits the terminal's stdio (use for progress output).
/// * `passthrough = false` — captures stderr; includes it in [`PackError::ToolFailed`].
fn invoke(
    exe: &Path,
    args: &[&str],
    _cwd: Option<&Path>,
    env_override: Option<(&str, &Path)>,
    passthrough: bool,
) -> Result<(), PackError> {
    let tool_name = exe
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or("tool")
        .to_string();

    let mut cmd = build_command(exe, args);

    if let Some((key, val)) = env_override {
        cmd.env(key, val);
    }

    if passthrough {
        let status = cmd.status().map_err(PackError::Io)?;
        if !status.success() {
            return Err(PackError::ToolFailed {
                tool: tool_name,
                code: status.code(),
                stderr: String::new(),
            });
        }
    } else {
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let output = cmd.output().map_err(PackError::Io)?;
        if !output.status.success() {
            return Err(PackError::ToolFailed {
                tool: tool_name,
                code: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr)
                    .trim()
                    .to_string(),
            });
        }
    }

    Ok(())
}

/// Recursively copy the contents of `src` into `dst`.
fn copy_dir_into(src: &Path, dst: &Path) -> Result<(), PackError> {
    for entry in WalkDir::new(src).min_depth(1) {
        let entry = entry?;
        let rel = entry.path().strip_prefix(src).expect("strip prefix failed");
        let target = dst.join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target)?;
        } else {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_when_tool_not_found() {
        let err =
            find_tool("cgbundle-nonexistent-tool-xyz-abc").unwrap_err();
        assert!(
            matches!(err, PackError::ToolNotFound(_)),
            "expected ToolNotFound, got {err:?}"
        );
    }

    #[test]
    fn copy_dir_into_copies_tree() {
        let src = tempfile::TempDir::new().unwrap();
        let dst = tempfile::TempDir::new().unwrap();

        std::fs::create_dir(src.path().join("sub")).unwrap();
        std::fs::write(src.path().join("a.txt"), b"hello").unwrap();
        std::fs::write(src.path().join("sub").join("b.txt"), b"world").unwrap();

        copy_dir_into(src.path(), dst.path()).unwrap();

        assert!(dst.path().join("a.txt").is_file());
        assert!(dst.path().join("sub").join("b.txt").is_file());
    }
}

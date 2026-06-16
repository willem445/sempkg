//! Build pipeline: run codegraph and LanceDB against source / docs directories,
//! then pack the results into a `.sembundle` archive.
//!
//! This is the implementation behind the `SemBundle build` subcommand.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use arrow_array::{RecordBatch, RecordBatchIterator, StringArray};
use arrow_schema::{DataType, Field, Schema};
use serde_json::json;
use walkdir::WalkDir;

use crate::error::PackError;
use crate::manifest::LanceMetadata;
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
    /// Where to write the finished `.sembundle`. Defaults to `./<name>-<version>.sembundle`.
    pub output_path: Option<PathBuf>,

    // --- CodeGraph inputs ---
    /// Source directories to index with `codegraph init --index`.
    /// At least one is required.
    pub source_dirs: Vec<PathBuf>,

    // --- Lance inputs (optional) ---
    /// Documentation directories to index with LanceDB. Empty = no lance extension.
    pub docs_dirs: Vec<PathBuf>,
    /// Glob mask for document discovery. Default: `**/*.{md,txt,rst}`.
    pub docs_glob: Option<String>,
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

    // Temporary working directory. Dropped (deleted) after pack() succeeds.
    let work = tempfile::TempDir::new()?;
    let cg_out = work.path().join("codegraph-out");
    std::fs::create_dir_all(&cg_out)?;

    // Step 1: index source directories with codegraph.
    eprintln!("[sembundle] Running codegraph ...");
    run_codegraph(&opts.source_dirs, &cg_out)?;

    // Step 2: index docs directories with LanceDB (optional).
    let lance_out = if !opts.docs_dirs.is_empty() {
        let lance_dir = work.path().join("lance-out");
        let glob = opts.docs_glob.as_deref().unwrap_or("**/*.{md,txt,rst}");
        eprintln!("[sembundle] Building LanceDB documentation index ...");
        run_lance(&opts.docs_dirs, &lance_dir, glob)?;
        Some(lance_dir)
    } else {
        None
    };

    // Step 3: pack.
    eprintln!("[sembundle] Packing bundle ...");
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
        lance_dir: lance_out,
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
            "[sembundle]   codegraph: indexing {} ...",
            source_dir.display()
        );
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
// LanceDB step
// ---------------------------------------------------------------------------

/// Build a LanceDB documentation index from `docs_dirs` and write it to `out_dir`.
///
/// The output directory will contain:
///   out_dir/metadata.json    — index metadata
///   out_dir/docs.lance/      — LanceDB table with tantivy FTS index
fn run_lance(
    docs_dirs: &[PathBuf],
    out_dir: &Path,
    glob_pattern: &str,
) -> Result<(), PackError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(PackError::Io)?;
    rt.block_on(run_lance_inner(docs_dirs, out_dir, glob_pattern))
}

async fn run_lance_inner(
    docs_dirs: &[PathBuf],
    out_dir: &Path,
    glob_pattern: &str,
) -> Result<(), PackError> {
    std::fs::create_dir_all(out_dir)?;

    let mut row_paths: Vec<String> = Vec::new();
    let mut row_contents: Vec<String> = Vec::new();
    let mut doc_count: u64 = 0;

    let patterns: Vec<&str> = glob_pattern.split(',').map(str::trim).collect();

    for docs_dir in docs_dirs {
        for entry in WalkDir::new(docs_dir)
            .min_depth(1)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
        {
            let path = entry.path();
            let rel = path.strip_prefix(docs_dir).unwrap_or(path);
            let rel_str = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");

            let matches = patterns.iter().any(|pat| {
                glob::Pattern::new(pat)
                    .map(|p| p.matches(&rel_str))
                    .unwrap_or(false)
            });
            if !matches {
                continue;
            }

            let text = match std::fs::read_to_string(path) {
                Ok(t) => t,
                Err(_) => continue,
            };

            doc_count += 1;
            for (i, chunk) in chunk_text(&text, 800).into_iter().enumerate() {
                row_paths.push(format!("{rel_str}#{i}"));
                row_contents.push(chunk);
            }
        }
    }

    let chunk_count = row_paths.len() as u64;

    if chunk_count == 0 {
        return Err(PackError::InvalidField {
            field: "docs_dirs".to_string(),
            reason: "no documents matched the glob pattern — check --docs-dir and --docs-glob"
                .to_string(),
        });
    }

    let schema = Arc::new(Schema::new(vec![
        Field::new("path", DataType::Utf8, false),
        Field::new("content", DataType::Utf8, false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from(row_paths)),
            Arc::new(StringArray::from(row_contents)),
        ],
    )
    .map_err(|e| PackError::InvalidField {
        field: "lance_batch".to_string(),
        reason: e.to_string(),
    })?;

    let reader = RecordBatchIterator::new(vec![Ok(batch)], schema);

    let db = lancedb::connect(out_dir.to_str().unwrap_or("."))
        .execute()
        .await
        .map_err(|e| PackError::InvalidField {
            field: "lancedb_connect".to_string(),
            reason: e.to_string(),
        })?;

    let tbl = db
        .create_table("docs", reader)
        .execute()
        .await
        .map_err(|e| PackError::InvalidField {
            field: "lancedb_create_table".to_string(),
            reason: e.to_string(),
        })?;

    let fts_ok = tbl
        .create_index(
            &["content"],
            lancedb::index::Index::FTS(
                lancedb::index::scalar::FtsIndexBuilder::default(),
            ),
        )
        .execute()
        .await
        .is_ok();

    if !fts_ok {
        eprintln!(
            "[sembundle] Warning: FTS index creation failed — search will use full scan."
        );
    }

    let metadata = LanceMetadata {
        table_name: "docs".to_string(),
        document_count: doc_count,
        chunk_count,
        indexed_paths: docs_dirs
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        fts_enabled: fts_ok,
        created_at: String::new(), // stamped by pack()
    };
    std::fs::write(
        out_dir.join("metadata.json"),
        serde_json::to_vec_pretty(&metadata)?,
    )?;

    eprintln!(
        "[sembundle]   lance: indexed {doc_count} documents, {chunk_count} chunks{}.",
        if fts_ok { " (FTS enabled)" } else { "" }
    );

    Ok(())
}

/// Split `text` into chunks of at most `max_chars` characters on paragraph boundaries.
fn chunk_text(text: &str, max_chars: usize) -> Vec<String> {
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();

    for para in text.split("\n\n") {
        let para = para.trim();
        if para.is_empty() {
            continue;
        }
        if current.len() + para.len() + 2 <= max_chars {
            if !current.is_empty() {
                current.push_str("\n\n");
            }
            current.push_str(para);
        } else {
            if !current.is_empty() {
                chunks.push(std::mem::take(&mut current));
            }
            if para.len() <= max_chars {
                current.push_str(para);
            } else {
                for window in para.as_bytes().chunks(max_chars) {
                    chunks.push(String::from_utf8_lossy(window).into_owned());
                }
            }
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    if chunks.is_empty() && !text.is_empty() {
        chunks.push(text.chars().take(max_chars).collect());
    }
    chunks
}

// ---------------------------------------------------------------------------
// Tool helpers
// ---------------------------------------------------------------------------

fn find_tool(name: &str) -> Result<PathBuf, PackError> {
    which::which(name).map_err(|_| PackError::ToolNotFound(name.to_string()))
}

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
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
    }

    Ok(())
}

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
        let err = find_tool("SemBundle-nonexistent-tool-xyz-abc").unwrap_err();
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

    #[test]
    fn chunk_text_splits_on_paragraphs() {
        let text = "para one\n\npara two\n\npara three";
        let chunks = chunk_text(text, 200);
        assert!(!chunks.is_empty());
        let joined = chunks.join(" ");
        assert!(joined.contains("para one"));
        assert!(joined.contains("para three"));
    }

    #[test]
    fn chunk_text_handles_oversized_paragraph() {
        let text = "x".repeat(2000);
        let chunks = chunk_text(&text, 800);
        assert!(chunks.len() >= 2);
        for c in &chunks {
            assert!(c.len() <= 800);
        }
    }
}

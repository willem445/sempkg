//! Build pipeline: run codegraph and LanceDB against source / docs directories,
//! then pack the results into a `.sembundle` archive.
//!
//! This is the implementation behind the `SemBundle build` subcommand.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use arrow_array::{RecordBatch, RecordBatchIterator, StringArray, UInt32Array};
use arrow_schema::{DataType, Field, Schema};
use serde_json::json;
use walkdir::WalkDir;

use crate::error::PackError;
use crate::manifest::{CodeMetadata, LanceMetadata};
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
    /// Glob mask for document discovery.
    /// Default: `**/*.md,**/*.txt,**/*.rst`.
    pub docs_glob: Option<String>,

    // --- Source-code index (optional) ---
    /// When `true`, build a LanceDB source-code index chunked by top-level symbols.
    pub include_source: bool,
    /// Glob mask restricting which source files are included in the code index.
    /// Default: `**/*.rs,**/*.py,**/*.ts,**/*.js,**/*.go,**/*.java,**/*.cpp,**/*.c,**/*.h`.
    pub source_glob: Option<String>,
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

    // Step 2: index docs directories with LanceDB (optional, best-effort).
    // When no documents match the glob pattern we log a warning and continue
    // without a LanceDB extension rather than failing the whole build.
    let lance_out = if !opts.docs_dirs.is_empty() {
        let lance_dir = work.path().join("lance-out");
        let glob = opts
            .docs_glob
            .as_deref()
            .unwrap_or("**/*.md,**/*.txt,**/*.rst");
        eprintln!("[sembundle] Building LanceDB documentation index ...");
        match run_lance(&opts.docs_dirs, &lance_dir, glob) {
            Ok(()) => Some(lance_dir),
            Err(PackError::InvalidField { ref field, .. }) if field == "docs_dirs" => {
                eprintln!(
                    "[sembundle] Warning: no documents matched the glob pattern — \
                     skipping LanceDB documentation index."
                );
                None
            }
            Err(e) => return Err(e),
        }
    } else {
        None
    };

    // Step 2b: build source-code LanceDB index (optional, --include-source).
    let code_out = if opts.include_source {
        let code_dir = work.path().join("code-out");
        let glob = opts
            .source_glob
            .as_deref()
            .unwrap_or("**/*.rs,**/*.py,**/*.ts,**/*.tsx,**/*.js,**/*.jsx,**/*.go,**/*.java,**/*.cpp,**/*.cc,**/*.cxx,**/*.c,**/*.h,**/*.hpp");
        eprintln!("[sembundle] Building LanceDB source-code index ...");
        match run_source_index(&opts.source_dirs, &code_dir, glob) {
            Ok(()) => Some(code_dir),
            Err(PackError::InvalidField { ref field, .. }) if field == "source_dirs" => {
                eprintln!(
                    "[sembundle] Warning: no source files matched the glob pattern — \
                     skipping source-code index."
                );
                None
            }
            Err(e) => return Err(e),
        }
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
        code_dir: code_out,
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

// ---------------------------------------------------------------------------
// Source-code index step
// ---------------------------------------------------------------------------

/// A single top-level symbol extracted from a source file.
struct SymbolChunk {
    /// Repo-relative source file path (forward slashes).
    path: String,
    /// Symbol name extracted from the declaration line.
    symbol: String,
    /// Symbol kind: `function`, `class`, `method`, `struct`, `enum`, `trait`,
    /// `impl`, `interface`, or `unknown`.
    kind: String,
    /// First non-blank line of the symbol body (the declaration / signature).
    signature: String,
    /// Full text of the symbol body.
    content: String,
    start_line: u32,
    end_line: u32,
    start_byte: u32,
    end_byte: u32,
}

/// Extract top-level symbol chunks from a single source file using a
/// language-agnostic line-scanner.  The heuristic detects declarations at
/// column 0 (or immediately after common visibility modifiers) and groups
/// the following lines as the symbol body until the next declaration or EOF.
///
/// Falls back to a single whole-file chunk when no symbols are detected.
fn extract_symbol_chunks(rel_path: &str, text: &str, max_chunk_bytes: usize) -> Vec<SymbolChunk> {
    // Patterns that signal the start of a top-level symbol, matched against
    // the leading token sequence on each line (after stripping visibility
    // modifiers / decorators).
    //
    // Each entry: (keyword, kind string)
    const KW: &[(&str, &str)] = &[
        // Rust
        ("fn ", "function"),
        ("async fn ", "function"),
        ("const fn ", "function"),
        ("unsafe fn ", "function"),
        ("extern \"C\" fn ", "function"),
        ("struct ", "struct"),
        ("enum ", "enum"),
        ("trait ", "trait"),
        ("impl ", "impl"),
        ("type ", "type"),
        ("mod ", "module"),
        // Python
        ("def ", "function"),
        ("async def ", "function"),
        ("class ", "class"),
        // JS / TS
        ("function ", "function"),
        ("function* ", "function"),
        ("async function ", "function"),
        ("class ", "class"),
        ("interface ", "interface"),
        ("type ", "type"),
        ("enum ", "enum"),
        // Go
        ("func ", "function"),
        ("type ", "type"),
        // Java / C#
        ("class ", "class"),
        ("interface ", "interface"),
        ("enum ", "enum"),
        ("record ", "class"),
    ];

    // Visibility / modifier prefixes to strip before matching keywords.
    const MODS: &[&str] = &[
        "pub(crate) ",
        "pub(super) ",
        "pub(in ",
        "pub ",
        "private ",
        "protected ",
        "public ",
        "static ",
        "abstract ",
        "final ",
        "override ",
        "export default ",
        "export ",
        "extern ",
        "inline ",
        "virtual ",
        "async ",
    ];

    /// Strip all leading modifier prefixes from a line to expose the keyword.
    fn strip_mods<'a>(line: &'a str, mods: &[&str]) -> &'a str {
        let mut s = line;
        loop {
            let mut changed = false;
            for m in mods {
                if let Some(rest) = s.strip_prefix(m) {
                    s = rest;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        s
    }

    /// Extract the symbol name: the first identifier token after the keyword.
    fn extract_name(stripped: &str, keyword: &str) -> String {
        let after = stripped.strip_prefix(keyword).unwrap_or(stripped);
        after
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .next()
            .unwrap_or("")
            .to_string()
    }

    struct Boundary {
        kind: String,
        name: String,
        line_idx: usize, // 0-based index into `lines`
        byte_offset: usize,
    }

    let lines: Vec<&str> = text.lines().collect();
    let mut boundaries: Vec<Boundary> = Vec::new();

    // Compute byte offset of each line.
    let mut byte_offsets: Vec<usize> = Vec::with_capacity(lines.len());
    let mut off = 0usize;
    for line in &lines {
        byte_offsets.push(off);
        off += line.len() + 1; // +1 for \n
    }

    for (i, line) in lines.iter().enumerate() {
        // Only consider lines at column 0 (top-level declarations).
        if line.starts_with(|c: char| c.is_whitespace()) {
            continue;
        }
        // Skip blank lines and comment-only lines.
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("/") || trimmed.starts_with("#") || trimmed.starts_with("//") {
            continue;
        }

        let stripped = strip_mods(line, MODS);
        for &(kw, kind_str) in KW {
            if stripped.starts_with(kw) {
                let name = extract_name(stripped, kw);
                if !name.is_empty() {
                    boundaries.push(Boundary {
                        kind: kind_str.to_string(),
                        name,
                        line_idx: i,
                        byte_offset: byte_offsets[i],
                    });
                    break;
                }
            }
        }
    }

    if boundaries.is_empty() {
        // No symbols detected — emit the whole file as one chunk.
        if text.is_empty() {
            return Vec::new();
        }
        let end_line = lines.len() as u32;
        let end_byte = text.len() as u32;
        let (sig, body) = split_signature(text);
        return vec![SymbolChunk {
            path: rel_path.to_string(),
            symbol: rel_path.to_string(),
            kind: "file".to_string(),
            signature: sig,
            content: body.chars().take(max_chunk_bytes).collect(),
            start_line: 1,
            end_line,
            start_byte: 0,
            end_byte,
        }];
    }

    let mut chunks: Vec<SymbolChunk> = Vec::new();
    for (idx, b) in boundaries.iter().enumerate() {
        let body_start_line = b.line_idx;
        let body_end_line = if idx + 1 < boundaries.len() {
            boundaries[idx + 1].line_idx.saturating_sub(1)
        } else {
            lines.len().saturating_sub(1)
        };

        let start_byte = b.byte_offset as u32;
        let end_byte = if body_end_line < lines.len() {
            (byte_offsets[body_end_line] + lines[body_end_line].len() + 1).min(text.len()) as u32
        } else {
            text.len() as u32
        };

        let body: String = lines[body_start_line..=body_end_line.min(lines.len().saturating_sub(1))]
            .join("\n");

        // Split oversized bodies into sub-chunks.
        if body.len() > max_chunk_bytes {
            let sub_chunks = split_body_into_windows(&body, max_chunk_bytes);
            let (sig, _) = split_signature(&body);
            for (sub_idx, sub) in sub_chunks.into_iter().enumerate() {
                chunks.push(SymbolChunk {
                    path: rel_path.to_string(),
                    symbol: format!("{}#{}", b.name, sub_idx),
                    kind: b.kind.clone(),
                    signature: if sub_idx == 0 { sig.clone() } else { String::new() },
                    content: sub,
                    start_line: (body_start_line + 1) as u32,
                    end_line: (body_end_line + 1) as u32,
                    start_byte,
                    end_byte,
                });
            }
        } else {
            let (sig, _) = split_signature(&body);
            chunks.push(SymbolChunk {
                path: rel_path.to_string(),
                symbol: b.name.clone(),
                kind: b.kind.clone(),
                signature: sig,
                content: body,
                start_line: (body_start_line + 1) as u32,
                end_line: (body_end_line + 1) as u32,
                start_byte,
                end_byte,
            });
        }
    }
    chunks
}

/// Returns `(signature_line, full_body)`. The signature is the first non-blank line.
fn split_signature(body: &str) -> (String, String) {
    let sig = body.lines().find(|l| !l.trim().is_empty()).unwrap_or("").to_string();
    (sig, body.to_string())
}

/// Split a long body string into windows of at most `max_bytes` characters.
fn split_body_into_windows(body: &str, max_bytes: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let bytes = body.as_bytes();
    while start < bytes.len() {
        let end = (start + max_bytes).min(bytes.len());
        // Snap to a char boundary.
        let mut boundary = end;
        while boundary > start && !body.is_char_boundary(boundary) {
            boundary -= 1;
        }
        out.push(body[start..boundary].to_string());
        start = boundary;
    }
    out
}

/// Build a LanceDB source-code index from `source_dirs` and write it to `out_dir`.
fn run_source_index(
    source_dirs: &[PathBuf],
    out_dir: &Path,
    glob_pattern: &str,
) -> Result<(), PackError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(PackError::Io)?;
    rt.block_on(run_source_inner(source_dirs, out_dir, glob_pattern))
}

async fn run_source_inner(
    source_dirs: &[PathBuf],
    out_dir: &Path,
    glob_pattern: &str,
) -> Result<(), PackError> {
    const MAX_CHUNK_BYTES: usize = 8 * 1024; // 8 KiB per symbol chunk

    std::fs::create_dir_all(out_dir)?;

    let mut row_paths: Vec<String> = Vec::new();
    let mut row_symbols: Vec<String> = Vec::new();
    let mut row_kinds: Vec<String> = Vec::new();
    let mut row_signatures: Vec<String> = Vec::new();
    let mut row_contents: Vec<String> = Vec::new();
    let mut row_start_lines: Vec<u32> = Vec::new();
    let mut row_end_lines: Vec<u32> = Vec::new();
    let mut row_start_bytes: Vec<u32> = Vec::new();
    let mut row_end_bytes: Vec<u32> = Vec::new();
    let mut symbol_count: u64 = 0;

    let patterns: Vec<&str> = glob_pattern.split(',').map(str::trim).collect();

    for source_dir in source_dirs {
        for entry in WalkDir::new(source_dir)
            .min_depth(1)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
        {
            let path = entry.path();
            let rel = path.strip_prefix(source_dir).unwrap_or(path);
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

            let file_chunks = extract_symbol_chunks(&rel_str, &text, MAX_CHUNK_BYTES);
            if !file_chunks.is_empty() {
                symbol_count += file_chunks.iter().filter(|c| !c.symbol.contains('#')).count() as u64;
                for c in file_chunks {
                    row_paths.push(c.path);
                    row_symbols.push(c.symbol);
                    row_kinds.push(c.kind);
                    row_signatures.push(c.signature);
                    row_contents.push(c.content);
                    row_start_lines.push(c.start_line);
                    row_end_lines.push(c.end_line);
                    row_start_bytes.push(c.start_byte);
                    row_end_bytes.push(c.end_byte);
                }
            }
        }
    }

    let chunk_count = row_paths.len() as u64;

    if chunk_count == 0 {
        return Err(PackError::InvalidField {
            field: "source_dirs".to_string(),
            reason: "no source files matched the glob pattern — check --source-dir and --source-glob"
                .to_string(),
        });
    }

    let schema = std::sync::Arc::new(Schema::new(vec![
        Field::new("path", DataType::Utf8, false),
        Field::new("symbol", DataType::Utf8, false),
        Field::new("kind", DataType::Utf8, false),
        Field::new("signature", DataType::Utf8, false),
        Field::new("content", DataType::Utf8, false),
        Field::new("start_line", DataType::UInt32, false),
        Field::new("end_line", DataType::UInt32, false),
        Field::new("start_byte", DataType::UInt32, false),
        Field::new("end_byte", DataType::UInt32, false),
    ]));

    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            std::sync::Arc::new(StringArray::from(row_paths)),
            std::sync::Arc::new(StringArray::from(row_symbols)),
            std::sync::Arc::new(StringArray::from(row_kinds)),
            std::sync::Arc::new(StringArray::from(row_signatures)),
            std::sync::Arc::new(StringArray::from(row_contents)),
            std::sync::Arc::new(UInt32Array::from(row_start_lines)),
            std::sync::Arc::new(UInt32Array::from(row_end_lines)),
            std::sync::Arc::new(UInt32Array::from(row_start_bytes)),
            std::sync::Arc::new(UInt32Array::from(row_end_bytes)),
        ],
    )
    .map_err(|e| PackError::InvalidField {
        field: "code_batch".to_string(),
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
        .create_table("code", reader)
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
            "[sembundle] Warning: code FTS index creation failed — search will use full scan."
        );
    }

    let metadata = CodeMetadata {
        table_name: "code".to_string(),
        symbol_count,
        chunk_count,
        indexed_paths: source_dirs
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
        "[sembundle]   code: indexed {symbol_count} symbols, {chunk_count} chunks{}.",
        if fts_ok { " (FTS enabled)" } else { "" }
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Doc chunker
// ---------------------------------------------------------------------------

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

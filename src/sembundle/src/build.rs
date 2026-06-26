//! Build pipeline: run codegraph and LanceDB against source / docs directories,
//! then pack the results into a `.sembundle` archive.
//!
//! This is the implementation behind the `SemBundle build` subcommand.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use arrow_array::{RecordBatch, StringArray, UInt32Array};
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

    // --- Exclusions (optional) ---
    /// Directories to exclude from all indexing (source, docs, and source-code index).
    /// Absolute paths are matched against entry paths directly; relative paths are
    /// matched against the entry's path relative to its base directory.
    pub exclude_dirs: Vec<PathBuf>,
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
    run_codegraph(&opts.source_dirs, &cg_out, &opts.exclude_dirs)?;

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
        match run_lance(&opts.docs_dirs, &lance_dir, glob, &opts.exclude_dirs) {
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
        // Prefer the codegraph.db that run_codegraph just produced.
        let cg_db = cg_out.join("graph").join("codegraph.db");
        let cg_db_opt = if cg_db.exists() { Some(cg_db.as_path()) } else { None };
        eprintln!("[sembundle] Building LanceDB source-code index ...");
        match run_source_index(&opts.source_dirs, &code_dir, glob, &opts.exclude_dirs, cg_db_opt) {
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

/// Returns `true` if `path` should be excluded based on `exclude_dirs`.
///
/// Absolute entries in `exclude_dirs` are compared against `path` directly.
/// Relative entries are compared against `path` stripped of `base_dir`.
fn is_excluded(path: &Path, base_dir: &Path, exclude_dirs: &[PathBuf]) -> bool {
    exclude_dirs.iter().any(|ex| {
        if ex.is_absolute() {
            path.starts_with(ex)
        } else {
            path.strip_prefix(base_dir)
                .map(|rel| rel.starts_with(ex))
                .unwrap_or(false)
        }
    })
}

fn run_codegraph(source_dirs: &[PathBuf], out_dir: &Path, exclude_dirs: &[PathBuf]) -> Result<(), PackError> {
    let exe = find_tool("codegraph")?;
    let graph_dir = out_dir.join("graph");
    std::fs::create_dir_all(&graph_dir)?;

    for source_dir in source_dirs {
        // Skip source_dirs that are themselves excluded.
        if !exclude_dirs.is_empty() && is_excluded(source_dir, source_dir, exclude_dirs) {
            eprintln!("[sembundle]   codegraph: skipping excluded dir {} ...", source_dir.display());
            continue;
        }
        eprintln!(
            "[sembundle]   codegraph: indexing {} ...",
            source_dir.display()
        );
        // `codegraph init --index` only indexes during first-time initialization.
        // If a `.codegraph/` directory already exists, `init` bails out with
        // "Already initialized" and performs no indexing, leaving the bundle with
        // a stale (or empty) graph. Detect that case and run a forced full
        // re-index instead, so every build produces a fresh, complete index.
        let src_str = source_dir.to_string_lossy();
        let dot_cg = source_dir.join(".codegraph");
        let args: Vec<&str> = if dot_cg.exists() {
            vec!["index", "--force", src_str.as_ref()]
        } else {
            vec!["init", "--index", src_str.as_ref()]
        };
        invoke(&exe, &args, None, None, true)?;

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
    exclude_dirs: &[PathBuf],
) -> Result<(), PackError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(PackError::Io)?;
    rt.block_on(run_lance_inner(docs_dirs, out_dir, glob_pattern, exclude_dirs))
}

async fn run_lance_inner(
    docs_dirs: &[PathBuf],
    out_dir: &Path,
    glob_pattern: &str,
    exclude_dirs: &[PathBuf],
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
            if !exclude_dirs.is_empty() && is_excluded(path, docs_dir, exclude_dirs) {
                continue;
            }
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

    let batches = vec![batch];

    let db = lancedb::connect(out_dir.to_str().unwrap_or("."))
        .execute()
        .await
        .map_err(|e| PackError::InvalidField {
            field: "lancedb_connect".to_string(),
            reason: e.to_string(),
        })?;

    let tbl = db
        .create_table("docs", batches)
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

/// Returns `true` when `trimmed` (already `.trim()`-ed) looks like a comment
/// line in any of the languages the code index supports.
///
/// Covers:
/// - `//`  `///`  `////`  — Rust / C / C++ / Java / JS / TS / Go / Swift
/// - `#`               — Python / Ruby / Shell / TOML / YAML / R
/// - `/*`  `*/`  `*`   — C-family block comments (middle lines start with `*`)
/// - `--`              — SQL / Lua / Haskell
/// - `%`               — MATLAB / LaTeX
fn is_comment_line(trimmed: &str) -> bool {
    trimmed.starts_with("//")
        || trimmed.starts_with('#')
        || trimmed.starts_with("/*")
        || trimmed.starts_with("*/")
        || trimmed.starts_with('*')
        || trimmed.starts_with("--")
        || trimmed.starts_with('%')
}

/// Walk backward from `symbol_start` (0-based index into `lines`) and return
/// the index of the first line of the comment block that immediately precedes
/// the symbol.
///
/// Algorithm:
/// 1. Allow **at most one** blank line between the symbol and the comment
///    block above (e.g. an empty separator line after the previous symbol).
/// 2. Collect contiguous comment lines walking upward until the first
///    non-comment line is reached — blank lines inside the comment block
///    are **not** crossed, so comments for a different symbol above a blank
///    separator are never captured.
///
/// Returns `symbol_start` unchanged when no qualifying comment is found.
fn collect_leading_comment_start(lines: &[&str], symbol_start: usize) -> usize {
    if symbol_start == 0 {
        return symbol_start;
    }
    let mut scan = symbol_start - 1;

    // Allow one blank separator line between the symbol and the comment.
    if lines[scan].trim().is_empty() {
        if scan == 0 {
            return symbol_start;
        }
        scan -= 1;
        // After the blank, the very next line must be a comment or we give up.
        if !is_comment_line(lines[scan].trim()) {
            return symbol_start;
        }
    } else if !is_comment_line(lines[scan].trim()) {
        return symbol_start;
    }

    // `scan` is now on a comment line.  Walk upward while lines stay comments.
    let mut comment_start = scan;
    while scan > 0 && is_comment_line(lines[scan - 1].trim()) {
        scan -= 1;
        comment_start = scan;
    }
    comment_start
}

/// Walk forward from `symbol_end` (0-based, inclusive) and return the index
/// of the last contiguous trailing comment line.
///
/// Only strictly adjacent lines are included — the first blank or non-comment
/// line terminates the scan.  This is intentionally conservative: trailing
/// comments are rare and we must not capture the leading comment of the next
/// symbol.
fn collect_trailing_comment_end(lines: &[&str], symbol_end: usize) -> usize {
    let mut comment_end = symbol_end;
    let mut scan = symbol_end + 1;
    while scan < lines.len() && is_comment_line(lines[scan].trim()) {
        comment_end = scan;
        scan += 1;
    }
    comment_end
}

/// A single symbol chunk ready to be written into the LanceDB `code` table.
struct SymbolChunk {
    path: String,
    symbol: String,
    kind: String,
    signature: String,
    content: String,
    start_line: u32,
    end_line: u32,
}

// ---------------------------------------------------------------------------
// Codegraph-DB driven extractor (primary path)
// ---------------------------------------------------------------------------

/// One row from the codegraph `nodes` SQLite table.
struct NodeRow {
    name: String,
    qualified_name: String,
    kind: String,
    file_path: String,
    start_line: u32,
    end_line: u32,
    signature: String,
}

/// Read every non-import, non-file node from `codegraph.db`, slice the
/// corresponding source lines from disk, and return the resulting chunks.
///
/// Each returned chunk has `start_line`/`end_line` that match the DB exactly,
/// so `read_symbol` / `read_code` in sempkg can locate them via a precise
/// `path + start_line` filter.
///
/// Bodies wider than `max_chunk_bytes` are split into numbered sub-chunks
/// (e.g. `MyFn#0`, `MyFn#1`) while preserving the original line range on
/// every sub-chunk row (so the location filter still finds the first chunk).
fn extract_chunks_from_codegraph_db(
    db_path: &Path,
    source_dirs: &[PathBuf],
    exclude_dirs: &[PathBuf],
    max_chunk_bytes: usize,
) -> Result<Vec<SymbolChunk>, PackError> {
    let conn = rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| PackError::InvalidField {
        field: "codegraph_db_open".to_string(),
        reason: e.to_string(),
    })?;

    // Exclude import/file nodes — they carry no meaningful source body.
    let mut stmt = conn
        .prepare(
            "SELECT name, COALESCE(qualified_name,''), kind, file_path, \
             COALESCE(start_line,0), COALESCE(end_line,0), COALESCE(signature,'') \
             FROM nodes \
             WHERE file_path IS NOT NULL AND file_path != '' \
               AND COALESCE(start_line,0) > 0 \
               AND COALESCE(end_line,0)   > 0 \
               AND kind NOT IN ('import','file','import_export') \
             ORDER BY file_path, start_line",
        )
        .map_err(|e| PackError::InvalidField {
            field: "codegraph_db_prepare".to_string(),
            reason: e.to_string(),
        })?;

    let rows: Vec<NodeRow> = stmt
        .query_map([], |row| {
            Ok(NodeRow {
                name: row.get(0)?,
                qualified_name: row.get(1)?,
                kind: row.get(2)?,
                file_path: row.get(3)?,
                start_line: row.get::<_, i64>(4)? as u32,
                end_line: row.get::<_, i64>(5)? as u32,
                signature: row.get(6)?,
            })
        })
        .map_err(|e| PackError::InvalidField {
            field: "codegraph_db_query".to_string(),
            reason: e.to_string(),
        })?
        .filter_map(|r| r.ok())
        .collect();

    // Group nodes by file_path so each source file is read exactly once.
    let mut by_file: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, row) in rows.iter().enumerate() {
        by_file.entry(row.file_path.clone()).or_default().push(i);
    }

    let mut chunks: Vec<SymbolChunk> = Vec::with_capacity(rows.len());

    'file: for (file_path, indices) in &by_file {
        // Resolve the relative path against each source_dir in order.
        let mut resolved: Option<(PathBuf, &PathBuf)> = None;
        for sd in source_dirs {
            // file_path uses forward slashes; Path::join handles them on all platforms.
            let candidate = sd.join(Path::new(file_path));
            if candidate.exists() {
                resolved = Some((candidate, sd));
                break;
            }
        }
        let (full_path, base_dir) = match resolved {
            Some(p) => p,
            None => continue 'file,
        };

        if !exclude_dirs.is_empty() && is_excluded(&full_path, base_dir, exclude_dirs) {
            continue 'file;
        }

        let text = match std::fs::read_to_string(&full_path) {
            Ok(t) => t,
            Err(_) => continue 'file,
        };
        let text_lines: Vec<&str> = text.lines().collect();
        let n_lines = text_lines.len();

        for &idx in indices {
            let row = &rows[idx];

            let s = row.start_line as usize;
            let e = row.end_line as usize;
            if s == 0 || s > n_lines {
                continue;
            }
            let start_idx = s - 1;
            let end_idx = (e - 1).min(n_lines.saturating_sub(1));

            // Extend the content window to include adjacent comment blocks.
            // start_line/end_line stored in the chunk remain the *symbol's*
            // own boundaries so that read_symbol / read_code location lookups
            // (which filter by path + start_line) are completely unaffected.
            let ctx_start = collect_leading_comment_start(&text_lines, start_idx);
            let ctx_end   = collect_trailing_comment_end(&text_lines, end_idx);
            let body: String = text_lines[ctx_start..=ctx_end].join("\n");

            // Signature comes from the first non-empty line of the symbol
            // itself (not the comment prefix), so skip leading comment lines.
            let sig = text_lines[start_idx..=end_idx]
                .iter()
                .find(|l| !l.trim().is_empty() && !is_comment_line(l.trim()))
                .copied()
                .unwrap_or("");

            // Prefer qualified_name; it is more unique (e.g. "Vec::push" vs "push").
            let sym_name = if !row.qualified_name.is_empty() {
                row.qualified_name.clone()
            } else {
                row.name.clone()
            };

            let sig = sig.to_string();

            if body.len() > max_chunk_bytes {
                // Sub-chunk oversized bodies; every sub-chunk keeps the same
                // start_line/end_line so the location filter still hits chunk #0.
                for (sub_idx, sub) in split_body_into_windows(&body, max_chunk_bytes)
                    .into_iter()
                    .enumerate()
                {
                    chunks.push(SymbolChunk {
                        path: file_path.clone(),
                        symbol: format!("{}#{}", sym_name, sub_idx),
                        kind: row.kind.clone(),
                        signature: if sub_idx == 0 { sig.clone() } else { String::new() },
                        content: sub,
                        start_line: row.start_line,
                        end_line: row.end_line,
                    });
                }
            } else {
                chunks.push(SymbolChunk {
                    path: file_path.clone(),
                    symbol: sym_name,
                    kind: row.kind.clone(),
                    signature: sig,
                    content: body,
                    start_line: row.start_line,
                    end_line: row.end_line,
                });
            }
        }
    }

    Ok(chunks)
}

// ---------------------------------------------------------------------------
// Line-scanner fallback (used when no codegraph.db is available)
// ---------------------------------------------------------------------------

/// Extract top-level symbol chunks using a language-agnostic line-scanner.
///
/// Used as a fallback when no `codegraph.db` is available.
fn extract_symbol_chunks(rel_path: &str, text: &str, max_chunk_bytes: usize) -> Vec<SymbolChunk> {
    const KW: &[(&str, &str)] = &[
        ("fn ", "function"), ("async fn ", "function"), ("const fn ", "function"),
        ("unsafe fn ", "function"), ("extern \"C\" fn ", "function"),
        ("struct ", "struct"), ("enum ", "enum"), ("trait ", "trait"),
        ("impl ", "impl"), ("type ", "type"), ("mod ", "module"),
        ("def ", "function"), ("async def ", "function"), ("class ", "class"),
        ("function ", "function"), ("function* ", "function"),
        ("async function ", "function"), ("interface ", "interface"),
        ("func ", "function"), ("record ", "class"),
    ];
    const MODS: &[&str] = &[
        "pub(crate) ", "pub(super) ", "pub(in ", "pub ", "private ", "protected ",
        "public ", "static ", "abstract ", "final ", "override ", "export default ",
        "export ", "extern ", "inline ", "virtual ", "async ",
    ];

    fn strip_mods<'a>(line: &'a str, mods: &[&str]) -> &'a str {
        let mut s = line;
        loop {
            let before = s;
            for m in mods { if let Some(r) = s.strip_prefix(m) { s = r; break; } }
            if s == before { break; }
        }
        s
    }

    fn extract_name(stripped: &str, keyword: &str) -> String {
        stripped.strip_prefix(keyword).unwrap_or(stripped)
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .next().unwrap_or("").to_string()
    }

    let lines: Vec<&str> = text.lines().collect();
    struct Boundary { kind: String, name: String, line_idx: usize }
    let mut boundaries: Vec<Boundary> = Vec::new();

    for (i, line) in lines.iter().enumerate() {
        if line.starts_with(|c: char| c.is_whitespace()) { continue; }
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('/') || trimmed.starts_with('#') { continue; }
        let stripped = strip_mods(line, MODS);
        for &(kw, kind_str) in KW {
            if stripped.starts_with(kw) {
                let name = extract_name(stripped, kw);
                if !name.is_empty() {
                    boundaries.push(Boundary { kind: kind_str.to_string(), name, line_idx: i });
                    break;
                }
            }
        }
    }

    if boundaries.is_empty() {
        if text.is_empty() { return Vec::new(); }
        let content: String = text.chars().take(max_chunk_bytes).collect();
        let sig = content.lines().find(|l| !l.trim().is_empty()).unwrap_or("").to_string();
        return vec![SymbolChunk {
            path: rel_path.to_string(), symbol: rel_path.to_string(),
            kind: "file".to_string(), signature: sig, content,
            start_line: 1, end_line: lines.len() as u32,
        }];
    }

    let mut chunks: Vec<SymbolChunk> = Vec::new();
    for (idx, b) in boundaries.iter().enumerate() {
        let start = b.line_idx;

        // Content ends just before the NEXT symbol's leading comments begin,
        // so each symbol owns its own comment block rather than stealing the
        // next symbol's documentation.
        let end = if idx + 1 < boundaries.len() {
            let next_ctx = collect_leading_comment_start(&lines, boundaries[idx + 1].line_idx);
            next_ctx.saturating_sub(1)
        } else {
            lines.len().saturating_sub(1)
        };

        // Walk upward from this symbol's first line to pick up any preceding
        // comment block.  The stored start_line reflects the symbol keyword
        // line, not the comment.
        let content_start = collect_leading_comment_start(&lines, start);
        let body: String = lines[content_start..=end.min(lines.len().saturating_sub(1))].join("\n");

        // Signature = first non-empty, non-comment line of the symbol itself.
        let sig = lines[start..=end.min(lines.len().saturating_sub(1))]
            .iter()
            .find(|l| !l.trim().is_empty() && !is_comment_line(l.trim()))
            .copied()
            .unwrap_or("");
        let start_line = (start + 1) as u32;
        let end_line = (end + 1) as u32;

        if body.len() > max_chunk_bytes {
            for (sub_idx, sub) in split_body_into_windows(&body, max_chunk_bytes).into_iter().enumerate() {
                chunks.push(SymbolChunk {
                    path: rel_path.to_string(),
                    symbol: format!("{}#{}", b.name, sub_idx),
                    kind: b.kind.clone(),
                    signature: if sub_idx == 0 { sig.to_string() } else { String::new() },
                    content: sub, start_line, end_line,
                });
            }
        } else {
            chunks.push(SymbolChunk {
                path: rel_path.to_string(), symbol: b.name.clone(),
                kind: b.kind.clone(), signature: sig.to_string(), content: body,
                start_line, end_line,
            });
        }
    }
    chunks
}

/// Split a long body string into windows of at most `max_bytes` characters.
fn split_body_into_windows(body: &str, max_bytes: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let bytes = body.as_bytes();
    while start < bytes.len() {
        let end = (start + max_bytes).min(bytes.len());
        let mut boundary = end;
        while boundary > start && !body.is_char_boundary(boundary) { boundary -= 1; }
        out.push(body[start..boundary].to_string());
        start = boundary;
    }
    out
}

// ---------------------------------------------------------------------------
// LanceDB writer
// ---------------------------------------------------------------------------

/// Build a LanceDB source-code index from `source_dirs` and write it to `out_dir`.
///
/// When `codegraph_db` is `Some`, symbols are read from the codegraph SQLite
/// database so each row's `start_line`/`end_line` aligns exactly with the
/// coordinates used by `read_symbol` and `read_code` in sempkg.
///
/// Falls back to a language-agnostic line-scanner when the DB is absent.
fn run_source_index(
    source_dirs: &[PathBuf],
    out_dir: &Path,
    glob_pattern: &str,
    exclude_dirs: &[PathBuf],
    codegraph_db: Option<&Path>,
) -> Result<(), PackError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(PackError::Io)?;
    rt.block_on(run_source_inner(source_dirs, out_dir, glob_pattern, exclude_dirs, codegraph_db))
}

async fn run_source_inner(
    source_dirs: &[PathBuf],
    out_dir: &Path,
    glob_pattern: &str,
    exclude_dirs: &[PathBuf],
    codegraph_db: Option<&Path>,
) -> Result<(), PackError> {
    const MAX_CHUNK_BYTES: usize = 8 * 1024; // 8 KiB per symbol chunk
    const BATCH_SIZE: usize = 500;            // rows per LanceDB write batch

    std::fs::create_dir_all(out_dir)?;

    // -----------------------------------------------------------------------
    // Extract chunks — codegraph DB path or line-scanner fallback
    // -----------------------------------------------------------------------
    let (chunks, used_db) = if let Some(db) = codegraph_db.filter(|p| p.exists()) {
        eprintln!("[sembundle]   code: reading symbols from codegraph.db ...");
        let c = extract_chunks_from_codegraph_db(db, source_dirs, exclude_dirs, MAX_CHUNK_BYTES)?;
        (c, true)
    } else {
        eprintln!("[sembundle]   code: codegraph.db not found — falling back to line-scanner ...");
        let mut acc: Vec<SymbolChunk> = Vec::new();
        let patterns: Vec<&str> = glob_pattern.split(',').map(str::trim).collect();
        for source_dir in source_dirs {
            for entry in WalkDir::new(source_dir).min_depth(1).into_iter()
                .filter_map(|e| e.ok()).filter(|e| e.file_type().is_file())
            {
                let path = entry.path();
                if !exclude_dirs.is_empty() && is_excluded(path, source_dir, exclude_dirs) { continue; }
                let rel = path.strip_prefix(source_dir).unwrap_or(path);
                let rel_str = rel.components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>().join("/");
                if !patterns.iter().any(|pat| glob::Pattern::new(pat).map(|p| p.matches(&rel_str)).unwrap_or(false)) {
                    continue;
                }
                let text = match std::fs::read_to_string(path) { Ok(t) => t, Err(_) => continue };
                acc.extend(extract_symbol_chunks(&rel_str, &text, MAX_CHUNK_BYTES));
            }
        }
        (acc, false)
    };

    let chunk_count = chunks.len() as u64;
    if chunk_count == 0 {
        return Err(PackError::InvalidField {
            field: "source_dirs".to_string(),
            reason: "no source files/symbols found — check --source-dir and --source-glob".to_string(),
        });
    }

    // Count primary symbols (not sub-chunks).
    let symbol_count: u64 = chunks.iter().filter(|c| !c.symbol.contains('#')).count() as u64;

    // -----------------------------------------------------------------------
    // Schema — drop start_byte/end_byte (reader treats them as optional/0)
    // -----------------------------------------------------------------------
    let schema = Arc::new(Schema::new(vec![
        Field::new("path",       DataType::Utf8,   false),
        Field::new("symbol",     DataType::Utf8,   false),
        Field::new("kind",       DataType::Utf8,   false),
        Field::new("signature",  DataType::Utf8,   false),
        Field::new("content",    DataType::Utf8,   false),
        Field::new("start_line", DataType::UInt32, false),
        Field::new("end_line",   DataType::UInt32, false),
    ]));

    // -----------------------------------------------------------------------
    // Write to LanceDB in BATCH_SIZE batches to avoid a single giant allocation
    // -----------------------------------------------------------------------
    let db = lancedb::connect(out_dir.to_str().unwrap_or("."))
        .execute()
        .await
        .map_err(|e| PackError::InvalidField { field: "lancedb_connect".to_string(), reason: e.to_string() })?;

    let make_batch = |slice: &[SymbolChunk]| -> Result<RecordBatch, PackError> {
        RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(StringArray::from(slice.iter().map(|c| c.path.as_str()).collect::<Vec<_>>())),
                Arc::new(StringArray::from(slice.iter().map(|c| c.symbol.as_str()).collect::<Vec<_>>())),
                Arc::new(StringArray::from(slice.iter().map(|c| c.kind.as_str()).collect::<Vec<_>>())),
                Arc::new(StringArray::from(slice.iter().map(|c| c.signature.as_str()).collect::<Vec<_>>())),
                Arc::new(StringArray::from(slice.iter().map(|c| c.content.as_str()).collect::<Vec<_>>())),
                Arc::new(UInt32Array::from(slice.iter().map(|c| c.start_line).collect::<Vec<_>>())),
                Arc::new(UInt32Array::from(slice.iter().map(|c| c.end_line).collect::<Vec<_>>())),
            ],
        )
        .map_err(|e| PackError::InvalidField { field: "code_batch".to_string(), reason: e.to_string() })
    };

    let mut batches = chunks.chunks(BATCH_SIZE);

    // First batch creates the table.
    let first = batches.next().expect("chunk_count > 0 guarantees at least one batch");
    let tbl = db
        .create_table("code", vec![make_batch(first)?])
        .execute()
        .await
        .map_err(|e| PackError::InvalidField { field: "lancedb_create_table".to_string(), reason: e.to_string() })?;

    // Subsequent batches are appended.
    for batch_slice in batches {
        tbl.add(vec![make_batch(batch_slice)?])
            .execute()
            .await
            .map_err(|e| PackError::InvalidField { field: "lancedb_add".to_string(), reason: e.to_string() })?;
    }

    // FTS index on the content column.
    let fts_ok = tbl
        .create_index(
            &["content"],
            lancedb::index::Index::FTS(lancedb::index::scalar::FtsIndexBuilder::default()),
        )
        .execute()
        .await
        .is_ok();

    if !fts_ok {
        eprintln!("[sembundle] Warning: code FTS index creation failed — search will use full scan.");
    }

    let metadata = CodeMetadata {
        table_name: "code".to_string(),
        symbol_count,
        chunk_count,
        indexed_paths: source_dirs.iter().map(|p| p.to_string_lossy().into_owned()).collect(),
        fts_enabled: fts_ok,
        created_at: String::new(), // stamped by pack()
    };
    std::fs::write(out_dir.join("metadata.json"), serde_json::to_vec_pretty(&metadata)?)?;

    eprintln!(
        "[sembundle]   code: indexed {symbol_count} symbols, {chunk_count} chunks{} (source: {}).",
        if fts_ok { " (FTS enabled)" } else { "" },
        if used_db { "codegraph.db" } else { "line-scanner" },
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

// ---------------------------------------------------------------------------

/// LanceDB documentation search — scoped to a specific bundle's embedded index.
///
/// Queries the LanceDB Arrow table (`lance/docs.lance/`) inside an extracted
/// bundle directory. All searches are strictly scoped to the bundle.
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow_array::{RecordBatch, RecordBatchIterator, StringArray, UInt32Array};
use arrow_schema::{DataType, Field, Schema};
use futures::TryStreamExt;
use lance_index::scalar::FullTextSearchQuery;
use lancedb::query::{ExecutableQuery, QueryBase};
use serde::Deserialize;

use crate::error::SempkgError;

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

pub fn lance_dir_path(bundle_dir: &Path) -> PathBuf {
    bundle_dir.join("lance")
}

pub fn lance_metadata_path(bundle_dir: &Path) -> PathBuf {
    bundle_dir.join("lance").join("metadata.json")
}

pub fn has_lance(bundle_dir: &Path) -> bool {
    lance_dir_path(bundle_dir).is_dir()
}

// ---------------------------------------------------------------------------
// LanceDB metadata
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct LanceMetadata {
    pub table_name: Option<String>,
    pub document_count: Option<u64>,
    pub chunk_count: Option<u64>,
    pub fts_enabled: Option<bool>,
    pub indexed_paths: Option<Vec<String>>,
    pub created_at: Option<String>,
}

pub fn load_metadata(lance_dir: &Path) -> Option<LanceMetadata> {
    let path = lance_dir.join("metadata.json");
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct SearchResult {
    pub path: String,
    pub snippet: String,
    /// 1-based line number where this chunk starts in the source file (0 = unknown).
    pub start_line: u32,
    /// 1-based line number where this chunk ends in the source file (0 = unknown).
    pub end_line: u32,
    /// Byte offset of the chunk start within the source file (0 = unknown).
    pub start_byte: u32,
    /// Byte offset of the chunk end within the source file (0 = unknown).
    pub end_byte: u32,
}

/// Full-text (BM25) search against the bundle's LanceDB table.
pub fn search(
    lance_dir: &Path,
    query: &str,
    limit: usize,
) -> crate::error::Result<Vec<SearchResult>> {
    if !lance_dir.is_dir() {
        return Err(SempkgError::NoLanceIndex(
            lance_dir.to_string_lossy().to_string(),
        ));
    }

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| SempkgError::Io(e))?;

    let results = rt.block_on(async {
        let db = lancedb::connect(lance_dir.to_str().unwrap_or("."))
            .execute()
            .await
            .map_err(|e| SempkgError::LanceError(e.to_string()))?;

        let tbl = db
            .open_table("docs")
            .execute()
            .await
            .map_err(|e| SempkgError::LanceError(e.to_string()))?;

        let batches: Vec<RecordBatch> = tbl
            .query()
            .full_text_search(
                FullTextSearchQuery::new(query.to_string()),
            )
            .limit(limit)
            .execute()
            .await
            .map_err(|e| SempkgError::LanceError(e.to_string()))?
            .try_collect()
            .await
            .map_err(|e| SempkgError::LanceError(e.to_string()))?;

        let mut out = Vec::new();
        for batch in &batches {
            let paths = batch
                .column_by_name("path")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());
            let contents = batch
                .column_by_name("content")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());

            let start_lines = batch
                .column_by_name("start_line")
                .and_then(|col| col.as_any().downcast_ref::<UInt32Array>());
            let end_lines = batch
                .column_by_name("end_line")
                .and_then(|col| col.as_any().downcast_ref::<UInt32Array>());
            let start_bytes = batch
                .column_by_name("start_byte")
                .and_then(|col| col.as_any().downcast_ref::<UInt32Array>());
            let end_bytes = batch
                .column_by_name("end_byte")
                .and_then(|col| col.as_any().downcast_ref::<UInt32Array>());

            if let (Some(p), Some(c)) = (paths, contents) {
                for i in 0..batch.num_rows() {
                    // Strip legacy "#chunk_index" suffix written by older index versions.
                    let raw_path = p.value(i);
                    let path = match raw_path.split_once('#') {
                        Some((f, _)) => f.to_string(),
                        None => raw_path.to_string(),
                    };
                    out.push(SearchResult {
                        path,
                        snippet: c.value(i).chars().take(400).collect(),
                        start_line: start_lines.map_or(0, |a| a.value(i)),
                        end_line:   end_lines  .map_or(0, |a| a.value(i)),
                        start_byte: start_bytes.map_or(0, |a| a.value(i)),
                        end_byte:   end_bytes  .map_or(0, |a| a.value(i)),
                    });
                }
            }
        }
        Ok::<Vec<SearchResult>, SempkgError>(out)
    })?;

    Ok(results)
}

/// Format search results as Markdown.
pub fn format_results(results: &[SearchResult], query: &str) -> String {
    if results.is_empty() {
        return format!("No results for '{query}'.");
    }
    results
        .iter()
        .map(|r| {
            let loc = if r.start_line > 0 {
                format!("{}:{}-{}", r.path, r.start_line, r.end_line)
            } else {
                r.path.clone()
            };
            format!("**{}**\n\n{}", loc, r.snippet)
        })
        .collect::<Vec<_>>()
        .join("\n\n---\n\n")
}

// ---------------------------------------------------------------------------
// CLI update: pure-Rust local indexing (no external tool required)
// ---------------------------------------------------------------------------

/// Split a glob spec on **top-level** commas (commas inside `{...}` are not
/// treated as separators), then expand any `{a,b,c}` brace groups in each
/// resulting token.
///
/// Examples:
/// - `"**/*.{md,rst,txt}"`       → `["**/*.md", "**/*.rst", "**/*.txt"]`
/// - `"**/*.md, **/*.rs"`        → `["**/*.md", "**/*.rs"]`
/// - `"**/*.{md,rst}, **/*.rs"`  → `["**/*.md", "**/*.rst", "**/*.rs"]`
fn expand_patterns(spec: &str) -> Vec<String> {
    // Split on top-level commas (depth tracks open braces).
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    for ch in spec.chars() {
        match ch {
            '{' => { depth += 1; current.push(ch); }
            '}' => { depth = depth.saturating_sub(1); current.push(ch); }
            ',' if depth == 0 => {
                let t = current.trim().to_string();
                if !t.is_empty() { tokens.push(t); }
                current.clear();
            }
            _ => { current.push(ch); }
        }
    }
    let t = current.trim().to_string();
    if !t.is_empty() { tokens.push(t); }

    // Expand brace groups in each token.
    tokens.into_iter().flat_map(|p| expand_braces(&p)).collect()
}

/// Expand a single `{a,b,c}` brace group inside a glob pattern.
/// Only the first brace group is expanded (sufficient for common patterns).
/// Returns the pattern unchanged as a single-element vec if no braces found.
fn expand_braces(pattern: &str) -> Vec<String> {
    if let Some(open) = pattern.find('{') {
        if let Some(close) = pattern[open..].find('}').map(|i| open + i) {
            let prefix = &pattern[..open];
            let suffix = &pattern[close + 1..];
            return pattern[open + 1..close]
                .split(',')
                .map(|alt| format!("{}{}{}", prefix, alt.trim(), suffix))
                .collect();
        }
    }
    vec![pattern.to_string()]
}

/// Walk `project_dir` with `glob_pattern`, chunk text, write a LanceDB table
/// at `<project_dir>/.sempkg/lance/`, and build a tantivy FTS index.
///
/// Returns the path to the lance directory on success.
pub fn cli_update(
    project_dir: &Path,
    _collection_name: &str,
    glob_pattern: &str,
) -> crate::error::Result<PathBuf> {
    let lance_out = project_dir.join(".sempkg").join("lance");
    std::fs::create_dir_all(&lance_out)?;

    let mut row_paths: Vec<String> = Vec::new();
    let mut row_contents: Vec<String> = Vec::new();
    let mut row_start_lines: Vec<u32> = Vec::new();
    let mut row_end_lines: Vec<u32> = Vec::new();
    let mut row_start_bytes: Vec<u32> = Vec::new();
    let mut row_end_bytes: Vec<u32> = Vec::new();
    let mut doc_count: u64 = 0;

    let patterns: Vec<String> = expand_patterns(glob_pattern);

    for entry in walkdir::WalkDir::new(project_dir)
        .min_depth(1)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let path = entry.path();
        let rel = path.strip_prefix(project_dir).unwrap_or(path);
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
        for chunk in chunk_text(&text, 800) {
            row_paths.push(rel_str.clone());
            row_contents.push(chunk.text);
            row_start_lines.push(chunk.start_line);
            row_end_lines.push(chunk.end_line);
            row_start_bytes.push(chunk.start_byte);
            row_end_bytes.push(chunk.end_byte);
        }
    }

    let chunk_count = row_paths.len() as u64;

    if chunk_count == 0 {
        return Err(SempkgError::LanceError(
            "no documents matched — check glob pattern".to_string(),
        ));
    }

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| SempkgError::Io(e))?;

    rt.block_on(async {
        let schema = Arc::new(Schema::new(vec![
            Field::new("path",       DataType::Utf8,   false),
            Field::new("content",    DataType::Utf8,   false),
            Field::new("start_line", DataType::UInt32, false),
            Field::new("end_line",   DataType::UInt32, false),
            Field::new("start_byte", DataType::UInt32, false),
            Field::new("end_byte",   DataType::UInt32, false),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(StringArray::from(row_paths)),
                Arc::new(StringArray::from(row_contents)),
                Arc::new(UInt32Array::from(row_start_lines)),
                Arc::new(UInt32Array::from(row_end_lines)),
                Arc::new(UInt32Array::from(row_start_bytes)),
                Arc::new(UInt32Array::from(row_end_bytes)),
            ],
        )
        .map_err(|e| SempkgError::LanceError(e.to_string()))?;

        let reader = RecordBatchIterator::new(vec![Ok::<RecordBatch, arrow_schema::ArrowError>(batch)], schema);

        let db = lancedb::connect(lance_out.to_str().unwrap_or("."))
            .execute()
            .await
            .map_err(|e| SempkgError::LanceError(e.to_string()))?;

        // Drop existing table if present so re-indexing works.
        let _ = db.drop_table("docs").await;

        let tbl = db
            .create_table("docs", reader)
            .execute()
            .await
            .map_err(|e| SempkgError::LanceError(e.to_string()))?;

        let _ = tbl
            .create_index(
                &["content"],
                lancedb::index::Index::FTS(
                    lancedb::index::scalar::FtsIndexBuilder::default(),
                ),
            )
            .execute()
            .await;

        Ok::<(), SempkgError>(())
    })?;

    // Write metadata.json.
    let meta = serde_json::json!({
        "table_name": "docs",
        "document_count": doc_count,
        "chunk_count": chunk_count,
        "fts_enabled": true,
        "indexed_paths": [project_dir.to_string_lossy()],
        "created_at": chrono::Utc::now().to_rfc3339(),
    });
    std::fs::write(
        lance_out.join("metadata.json"),
        serde_json::to_vec_pretty(&meta)
            .map_err(SempkgError::Json)?,
    )?;

    eprintln!(
        "[sempkg] lance: indexed {doc_count} documents, {chunk_count} chunks."
    );

    Ok(lance_out)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

struct ChunkInfo {
    text: String,
    /// 1-based line number of the first line of this chunk.
    start_line: u32,
    /// 1-based line number of the last line of this chunk.
    end_line: u32,
    /// Byte offset of the chunk's start within the source file.
    start_byte: u32,
    /// Byte offset of the chunk's end (exclusive) within the source file.
    end_byte: u32,
}

/// Map a byte offset to its 1-based line number within `text`.
///
/// Snaps `byte_offset` back to the nearest valid UTF-8 char boundary so
/// callers do not need to worry about multi-byte codepoints.
fn byte_to_line(text: &str, byte_offset: usize) -> u32 {
    let mut boundary = byte_offset.min(text.len());
    while boundary > 0 && !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    text[..boundary]
        .bytes()
        .filter(|&b| b == b'\n')
        .count() as u32
        + 1
}

fn chunk_text(text: &str, max_chars: usize) -> Vec<ChunkInfo> {
    // Collect paragraphs (split on \n\n) with their byte offsets in `text`.
    // `str::split` returns subslices of the original, so pointer arithmetic
    // correctly gives us the byte offset of each paragraph.
    let mut paras: Vec<(usize, usize, &str)> = Vec::new();
    for raw in text.split("\n\n") {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let raw_start = raw.as_ptr() as usize - text.as_ptr() as usize;
        let trim_lead = raw.len() - raw.trim_start().len();
        let start_byte = raw_start + trim_lead;
        let end_byte = start_byte + trimmed.len();
        paras.push((start_byte, end_byte, trimmed));
    }

    let mut chunks: Vec<ChunkInfo> = Vec::new();
    let mut cur_text = String::new();
    let mut cur_start = 0usize;
    let mut cur_end = 0usize;

    for &(start_byte, end_byte, para) in &paras {
        let fits = if cur_text.is_empty() {
            para.len() <= max_chars
        } else {
            cur_text.len() + 2 + para.len() <= max_chars
        };

        if fits {
            if cur_text.is_empty() {
                cur_start = start_byte;
            } else {
                cur_text.push_str("\n\n");
            }
            cur_text.push_str(para);
            cur_end = end_byte;
        } else {
            // Flush accumulated chunk.
            if !cur_text.is_empty() {
                chunks.push(ChunkInfo {
                    start_line: byte_to_line(text, cur_start),
                    end_line:   byte_to_line(text, cur_end.saturating_sub(1)),
                    start_byte: cur_start as u32,
                    end_byte:   cur_end as u32,
                    text: std::mem::take(&mut cur_text),
                });
            }
            if para.len() <= max_chars {
                cur_text.push_str(para);
                cur_start = start_byte;
                cur_end = end_byte;
            } else {
                // Oversized paragraph: split into byte windows.
                let mut off = 0usize;
                for window in para.as_bytes().chunks(max_chars) {
                    let w_start = start_byte + off;
                    let w_end = w_start + window.len();
                    chunks.push(ChunkInfo {
                        text: String::from_utf8_lossy(window).into_owned(),
                        start_line: byte_to_line(text, w_start),
                        end_line:   byte_to_line(text, w_end.saturating_sub(1)),
                        start_byte: w_start as u32,
                        end_byte:   w_end as u32,
                    });
                    off += window.len();
                }
            }
        }
    }
    if !cur_text.is_empty() {
        chunks.push(ChunkInfo {
            start_line: byte_to_line(text, cur_start),
            end_line:   byte_to_line(text, cur_end.saturating_sub(1)),
            start_byte: cur_start as u32,
            end_byte:   cur_end as u32,
            text: cur_text,
        });
    }
    if chunks.is_empty() && !text.is_empty() {
        let end = text.len().min(max_chars);
        chunks.push(ChunkInfo {
            text: text.chars().take(max_chars).collect(),
            start_line: 1,
            end_line:   byte_to_line(text, end),
            start_byte: 0,
            end_byte:   end as u32,
        });
    }
    chunks
}

/// LanceDB documentation search — scoped to a specific bundle's embedded index.
///
/// Queries the LanceDB Arrow table (`lance/docs.lance/`) inside an extracted
/// bundle directory. All searches are strictly scoped to the bundle.
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow_array::{RecordBatch, RecordBatchIterator, StringArray};
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

            if let (Some(p), Some(c)) = (paths, contents) {
                for i in 0..batch.num_rows() {
                    out.push(SearchResult {
                        path: p.value(i).to_string(),
                        snippet: c.value(i).chars().take(400).collect(),
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
        .map(|r| format!("**{}**\n\n{}", r.path, r.snippet))
        .collect::<Vec<_>>()
        .join("\n\n---\n\n")
}

// ---------------------------------------------------------------------------
// CLI update: pure-Rust local indexing (no external tool required)
// ---------------------------------------------------------------------------

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
    let mut doc_count: u64 = 0;

    let patterns: Vec<&str> = glob_pattern.split(',').map(str::trim).collect();

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
        for (i, chunk) in chunk_text(&text, 800).into_iter().enumerate() {
            row_paths.push(format!("{rel_str}#{i}"));
            row_contents.push(chunk);
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

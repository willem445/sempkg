/// LanceDB documentation search — scoped to a specific bundle's embedded index.
///
/// Queries the LanceDB Arrow table (`lance/docs.lance/`) inside an extracted
/// bundle directory. All searches are strictly scoped to the bundle.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow_array::{Array, FixedSizeListArray, Float32Array, RecordBatch, StringArray, UInt32Array};
use arrow_schema::{DataType, Field, Schema};
use futures::TryStreamExt;
use lancedb::index::scalar::FullTextSearchQuery;
use lancedb::query::{ExecutableQuery, QueryBase};
use lancedb::DistanceType;
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
    /// Identifier of the embedding model used to populate the `vector` column,
    /// if vectors are present. Written by `sempkg embed`.
    #[serde(default)]
    pub embedding_model: Option<String>,
    /// Dimension of the stored vectors, if present.
    #[serde(default)]
    pub embedding_dim: Option<u32>,
}

pub fn load_metadata(lance_dir: &Path) -> Option<LanceMetadata> {
    let path = lance_dir.join("metadata.json");
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Read the embedding model id + dimension recorded in a table's
/// `metadata.json` (works for both `lance/` and `code/` directories).
///
/// Returns `None` when the directory has no metadata or no embeddings.
pub fn read_embedding_info(table_dir: &Path) -> Option<(String, u32)> {
    let meta = load_metadata(table_dir)?;
    match (meta.embedding_model, meta.embedding_dim) {
        (Some(model), Some(dim)) => Some((model, dim)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
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
    // Code-index fields (absent for docs results).
    pub symbol: Option<String>,
    pub kind: Option<String>,
    pub signature: Option<String>,
}

/// Full source body for a single symbol, returned by `fetch_symbol_source`.
#[derive(Debug)]
pub struct SymbolSource {
    pub path: String,
    pub symbol: String,
    pub kind: String,
    pub signature: String,
    pub content: String,
    pub start_line: u32,
    pub end_line: u32,
}

/// A slice of documentation content for a single file, returned by
/// [`fetch_doc_lines`]. It concatenates one or more stored doc chunks so an
/// agent can read wider raw context than the truncated search snippet.
#[derive(Debug)]
pub struct DocSlice {
    pub path: String,
    pub content: String,
    /// 1-based first line covered by the returned content (0 = unknown).
    pub start_line: u32,
    /// 1-based last line covered by the returned content (0 = unknown).
    pub end_line: u32,
    /// Number of stored chunks combined into `content`.
    pub chunk_count: usize,
    /// True when the docs table had no line-range columns (e.g. an older
    /// bundle), so any requested line range could not be applied.
    pub line_meta_missing: bool,
}

/// A lightweight description of a symbol candidate used when a name is ambiguous.
#[derive(Debug, Clone)]
pub struct SymbolCandidate {
    pub name: String,
    pub qualified_name: String,
    pub kind: String,
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
}

/// Result of a `fetch_symbol_source` lookup.
///
/// - `Unique`      — exactly one match; contains the full source.
/// - `Ambiguous`   — multiple nodes share the same name; the caller must ask
///                   the user to disambiguate.
/// - `NotFound`    — no node matched the requested symbol name.
#[derive(Debug)]
pub enum SymbolLookup {
    Unique(SymbolSource),
    Ambiguous(Vec<SymbolCandidate>),
    NotFound,
}

pub fn code_dir_path(bundle_dir: &Path) -> PathBuf {
    bundle_dir.join("code")
}

pub fn has_code(bundle_dir: &Path) -> bool {
    code_dir_path(bundle_dir).is_dir()
}

/// Full-text (BM25) search against the docs LanceDB table.
pub fn search(
    lance_dir: &Path,
    query: &str,
    limit: usize,
) -> crate::error::Result<Vec<SearchResult>> {
    search_table(lance_dir, "docs", query, limit, false)
}

/// Full-text (BM25) search against the code LanceDB table.
pub fn search_code(
    code_dir: &Path,
    query: &str,
    limit: usize,
) -> crate::error::Result<Vec<SearchResult>> {
    search_table(code_dir, "code", query, limit, true)
}

/// Internal: BM25 search against a named LanceDB table.  When `is_code` is
/// true the reader also extracts symbol/kind/signature columns.
fn search_table(
    dir: &Path,
    table_name: &str,
    query: &str,
    limit: usize,
    is_code: bool,
) -> crate::error::Result<Vec<SearchResult>> {
    if !dir.is_dir() {
        return Err(SempkgError::NoLanceIndex(dir.to_string_lossy().to_string()));
    }

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| SempkgError::Io(e))?;

    let results = rt.block_on(async {
        let db = lancedb::connect(dir.to_str().unwrap_or("."))
            .execute()
            .await
            .map_err(|e| SempkgError::LanceError(e.to_string()))?;

        let tbl = db
            .open_table(table_name)
            .execute()
            .await
            .map_err(|e| SempkgError::LanceError(e.to_string()))?;

        let batches: Vec<RecordBatch> = tbl
            .query()
            .full_text_search(FullTextSearchQuery::new(query.to_string()))
            .limit(limit)
            .execute()
            .await
            .map_err(|e| SempkgError::LanceError(e.to_string()))?
            .try_collect()
            .await
            .map_err(|e| SempkgError::LanceError(e.to_string()))?;

        Ok::<Vec<SearchResult>, SempkgError>(extract_results(&batches, is_code))
    })?;

    Ok(results)
}

/// Internal: convert LanceDB result batches into `SearchResult`s. Shared by the
/// BM25 (`search_table`) and vector (`search_vector_table`) search paths.
fn extract_results(batches: &[RecordBatch], is_code: bool) -> Vec<SearchResult> {
    let mut out = Vec::new();
    for batch in batches {
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

        let symbols = if is_code {
            batch
                .column_by_name("symbol")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>())
        } else {
            None
        };
        let kinds = if is_code {
            batch
                .column_by_name("kind")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>())
        } else {
            None
        };
        let signatures = if is_code {
            batch
                .column_by_name("signature")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>())
        } else {
            None
        };

        if let (Some(p), Some(c)) = (paths, contents) {
            for i in 0..batch.num_rows() {
                let raw_path = p.value(i);
                let path = match raw_path.split_once('#') {
                    Some((f, _)) => f.to_string(),
                    None => raw_path.to_string(),
                };
                let snippet_len = if is_code { 600 } else { 400 };
                out.push(SearchResult {
                    path,
                    snippet: c.value(i).chars().take(snippet_len).collect(),
                    start_line: start_lines.map_or(0, |a| a.value(i)),
                    end_line: end_lines.map_or(0, |a| a.value(i)),
                    start_byte: start_bytes.map_or(0, |a| a.value(i)),
                    end_byte: end_bytes.map_or(0, |a| a.value(i)),
                    symbol: symbols.map(|a| a.value(i).to_string()),
                    kind: kinds.map(|a| a.value(i).to_string()),
                    signature: signatures.map(|a| a.value(i).to_string()),
                });
            }
        }
    }
    out
}

/// Vector (semantic) search against the docs LanceDB table.
pub fn search_vector(
    lance_dir: &Path,
    query_vec: &[f32],
    limit: usize,
) -> crate::error::Result<Vec<SearchResult>> {
    search_vector_table(lance_dir, "docs", query_vec, limit, false)
}

/// Vector (semantic) search against the code LanceDB table.
pub fn search_code_vector(
    code_dir: &Path,
    query_vec: &[f32],
    limit: usize,
) -> crate::error::Result<Vec<SearchResult>> {
    search_vector_table(code_dir, "code", query_vec, limit, true)
}

/// Internal: cosine vector search against a named LanceDB table that has a
/// `vector` column. Returns an empty result set (not an error) when the table
/// has no `vector` column so callers degrade gracefully to BM25-only.
fn search_vector_table(
    dir: &Path,
    table_name: &str,
    query_vec: &[f32],
    limit: usize,
    is_code: bool,
) -> crate::error::Result<Vec<SearchResult>> {
    if !dir.is_dir() {
        return Err(SempkgError::NoLanceIndex(dir.to_string_lossy().to_string()));
    }

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(SempkgError::Io)?;

    let query_vec = query_vec.to_vec();
    let results = rt.block_on(async {
        let db = lancedb::connect(dir.to_str().unwrap_or("."))
            .execute()
            .await
            .map_err(|e| SempkgError::LanceError(e.to_string()))?;

        let tbl = db
            .open_table(table_name)
            .execute()
            .await
            .map_err(|e| SempkgError::LanceError(e.to_string()))?;

        // If the table has no `vector` column, there are no embeddings to query.
        let schema = tbl
            .schema()
            .await
            .map_err(|e| SempkgError::LanceError(e.to_string()))?;
        if schema.column_with_name("vector").is_none() {
            return Ok::<Vec<SearchResult>, SempkgError>(Vec::new());
        }

        let batches: Vec<RecordBatch> = tbl
            .query()
            .nearest_to(query_vec)
            .map_err(|e| SempkgError::LanceError(e.to_string()))?
            .column("vector")
            .distance_type(DistanceType::Cosine)
            .limit(limit)
            .execute()
            .await
            .map_err(|e| SempkgError::LanceError(e.to_string()))?
            .try_collect()
            .await
            .map_err(|e| SempkgError::LanceError(e.to_string()))?;

        Ok::<Vec<SearchResult>, SempkgError>(extract_results(&batches, is_code))
    })?;

    Ok(results)
}

/// Embed every row's `content` in a LanceDB table and rewrite the table with an
/// added `vector` column, then stamp the embedding model id + dimension into the
/// table's `metadata.json`.
///
/// Works for both the `docs` and `code` tables: all existing columns are
/// preserved and a `vector` `FixedSizeList<Float32, dim>` column is appended.
/// Vector search uses LanceDB's brute-force kNN (no ANN index is built, which
/// keeps bundle-sized tables exact and avoids "not enough rows to train"
/// failures).
pub fn embed_table(
    dir: &Path,
    table_name: &str,
    embedder: &crate::embedding::Embedder,
    model_id: &str,
) -> crate::error::Result<u64> {
    if !dir.is_dir() {
        return Err(SempkgError::NoLanceIndex(dir.to_string_lossy().to_string()));
    }

    let dim = embedder.dim();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(SempkgError::Io)?;

    let row_count = rt.block_on(async {
        let db = lancedb::connect(dir.to_str().unwrap_or("."))
            .execute()
            .await
            .map_err(|e| SempkgError::LanceError(e.to_string()))?;

        let tbl = db
            .open_table(table_name)
            .execute()
            .await
            .map_err(|e| SempkgError::LanceError(e.to_string()))?;

        // Full scan of the existing table (a plain query with no limit returns
        // every row).
        let batches: Vec<RecordBatch> = tbl
            .query()
            .execute()
            .await
            .map_err(|e| SempkgError::LanceError(e.to_string()))?
            .try_collect()
            .await
            .map_err(|e| SempkgError::LanceError(e.to_string()))?;

        if batches.is_empty() {
            return Ok::<u64, SempkgError>(0);
        }

        // Build the augmented schema (original fields + `vector`).
        let original_schema = batches[0].schema();
        let item_field = Arc::new(Field::new("item", DataType::Float32, true));
        let vector_field = Field::new(
            "vector",
            DataType::FixedSizeList(item_field.clone(), dim as i32),
            true,
        );
        let mut fields: Vec<Arc<Field>> = original_schema.fields().iter().cloned().collect();
        fields.push(Arc::new(vector_field));
        let new_schema = Arc::new(Schema::new(fields));

        // Collect every content string from all Arrow batches so we can embed
        // them all in one batched call (one context creation, multi-seq decode).
        let all_texts: Vec<String> = batches
            .iter()
            .flat_map(|b| {
                let col = b
                    .column_by_name("content")
                    .and_then(|c| c.as_any().downcast_ref::<StringArray>());
                (0..b.num_rows())
                    .map(move |i| col.map(|c| c.value(i).to_owned()).unwrap_or_default())
            })
            .collect();

        let total_rows = all_texts.len() as u64;
        println!("    embedding {total_rows} rows...");

        // Validate content column exists in the first non-empty batch.
        if batches
            .iter()
            .any(|b| b.column_by_name("content").is_none())
        {
            return Err(SempkgError::LanceError(format!(
                "table `{table_name}` has no `content` column to embed"
            )));
        }

        // Embed all rows in one batched pass (context created once inside).
        let all_vecs = embedder
            .embed_documents_batch(&all_texts)
            .map_err(|e| SempkgError::LanceError(format!("batch embedding: {e}")))?;

        // Verify dim on the first result.
        if let Some(v) = all_vecs.first() {
            if v.len() != dim {
                return Err(SempkgError::LanceError(format!(
                    "embedding dim mismatch: got {} expected {dim}",
                    v.len()
                )));
            }
        }

        // Distribute embeddings back into per-Arrow-batch flat arrays and
        // rebuild each RecordBatch with the new `vector` column appended.
        let mut embed_iter = all_vecs.into_iter();
        let mut new_batches: Vec<RecordBatch> = Vec::with_capacity(batches.len());

        for batch in &batches {
            let n = batch.num_rows();
            let mut flat: Vec<f32> = Vec::with_capacity(n * dim);
            for _ in 0..n {
                let vec = embed_iter.next().unwrap_or_default();
                flat.extend_from_slice(&vec);
            }

            let values = Float32Array::from(flat);
            let list =
                FixedSizeListArray::new(item_field.clone(), dim as i32, Arc::new(values), None);

            let mut columns = batch.columns().to_vec();
            columns.push(Arc::new(list));
            let new_batch = RecordBatch::try_new(new_schema.clone(), columns)
                .map_err(|e| SempkgError::LanceError(e.to_string()))?;
            new_batches.push(new_batch);
        }

        // Replace the table with the embedded version.
        let _ = db.drop_table(table_name, &[]).await;
        let tbl = db
            .create_table(table_name, new_batches)
            .execute()
            .await
            .map_err(|e| SempkgError::LanceError(e.to_string()))?;

        // Recreate the FTS index on `content` (best effort).
        let _ = tbl
            .create_index(
                &["content"],
                lancedb::index::Index::FTS(lancedb::index::scalar::FtsIndexBuilder::default()),
            )
            .execute()
            .await;

        Ok::<u64, SempkgError>(total_rows)
    })?;

    // Stamp embedding metadata into the table's metadata.json.
    stamp_embedding_metadata(dir, model_id, dim as u32)?;

    Ok(row_count)
}

/// Update (or create) `<dir>/metadata.json` with the embedding model id and
/// dimension, preserving any existing fields.
fn stamp_embedding_metadata(dir: &Path, model_id: &str, dim: u32) -> crate::error::Result<()> {
    let meta_path = dir.join("metadata.json");
    let mut value: serde_json::Value = std::fs::read_to_string(&meta_path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| serde_json::json!({}));

    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "embedding_model".to_string(),
            serde_json::Value::String(model_id.to_string()),
        );
        obj.insert(
            "embedding_dim".to_string(),
            serde_json::Value::Number(dim.into()),
        );
    }

    std::fs::write(
        &meta_path,
        serde_json::to_vec_pretty(&value).map_err(SempkgError::Json)?,
    )?;
    Ok(())
}

/// Fetch the full source body for a symbol from the `code` table.
///
/// Resolution path:
/// 1. Query `codegraph.db` (via the `graph/` sibling of `code_dir`) for an
///    exact name / qualified-name match — this gives precise `file_path`,
///    `start_line`, `end_line`.
/// 2. If more than one node matches the name exactly, return
///    [`SymbolLookup::Ambiguous`] with a candidate list so the caller can ask
///    the user to disambiguate via `read_code(file, line)`.
/// 3. Query the LanceDB `code` table with an exact `path + start_line` filter
///    to retrieve the stored content without any BM25 / FTS ambiguity.
/// 4. If the stored chunk is wider than the codegraph range, slice it to the
///    exact lines.
pub fn fetch_symbol_source(code_dir: &Path, symbol: &str) -> crate::error::Result<SymbolLookup> {
    if !code_dir.is_dir() {
        return Ok(SymbolLookup::NotFound);
    }

    // Derive the codegraph.db path from the bundle directory (parent of code/).
    let db_path = code_dir
        .parent()
        .map(|p| p.join("graph").join("codegraph.db"))
        .filter(|p| p.exists());

    // --- Step 1: resolve symbol location via SQLite ---
    let nodes: Vec<crate::codegraph::NodeRecord> = if let Some(ref db) = db_path {
        crate::codegraph::db_query_symbol_all(db, symbol).unwrap_or_default()
    } else {
        Vec::new()
    };

    // --- Step 2: check for ambiguity ---
    if nodes.len() > 1 {
        let candidates = nodes
            .into_iter()
            .map(|n| SymbolCandidate {
                qualified_name: n.qualified_name.clone(),
                name: n.name,
                kind: n.kind,
                path: n.file_path,
                start_line: n.start_line,
                end_line: n.end_line,
            })
            .collect();
        return Ok(SymbolLookup::Ambiguous(candidates));
    }

    let node = nodes.into_iter().next();

    // --- Step 3: find the LanceDB row with an exact path+start_line filter ---
    match fetch_from_code_table(code_dir, node.as_ref(), symbol)? {
        Some(src) => Ok(SymbolLookup::Unique(src)),
        None => Ok(SymbolLookup::NotFound),
    }
}

/// Fetch the symbol whose source range contains `line` in `file`.
///
/// Resolution path:
/// 1. Query `codegraph.db` (SQLite) for the tightest enclosing symbol at
///    `file:line` — gives precise `file_path`, `start_line`, `end_line`.
/// 2. Query the LanceDB `code` table with an exact `path + start_line` filter.
/// 3. Slice the content to the codegraph range if the stored chunk is wider.
pub fn fetch_symbol_at_location(
    code_dir: &Path,
    file: &str,
    line: u32,
) -> crate::error::Result<Option<SymbolSource>> {
    if !code_dir.is_dir() {
        return Ok(None);
    }

    let db_path = code_dir
        .parent()
        .map(|p| p.join("graph").join("codegraph.db"))
        .filter(|p| p.exists());

    let node = if let Some(ref db) = db_path {
        crate::codegraph::db_query_at_location(db, file, line)
            .ok()
            .flatten()
    } else {
        None
    };

    // Fall back to a path-suffix + line-range filter when no SQLite DB is available.
    if node.is_none() && db_path.is_none() {
        return fetch_at_location_lance_fallback(code_dir, file, line);
    }

    fetch_from_code_table(code_dir, node.as_ref(), file)
}

/// Internal: given a resolved `NodeRecord` (or `None`), query the LanceDB
/// `code` table by exact `path + start_line` and return the tightest content.
///
/// When `node` is `Some`, we filter by the node's `file_path` + `start_line`.
/// When `node` is `None` and only a `hint` (symbol name or file path) is
/// provided, we attempt a name-based fallback via a wider filter.
fn fetch_from_code_table(
    code_dir: &Path,
    node: Option<&crate::codegraph::NodeRecord>,
    hint: &str,
) -> crate::error::Result<Option<SymbolSource>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(SempkgError::Io)?;

    let result = rt.block_on(async {
        let db = lancedb::connect(code_dir.to_str().unwrap_or("."))
            .execute()
            .await
            .map_err(|e| SempkgError::LanceError(e.to_string()))?;

        let tbl = db
            .open_table("code")
            .execute()
            .await
            .map_err(|e| SempkgError::LanceError(e.to_string()))?;

        // Build the filter expression from the resolved node or fall back to a
        // full-text search on the symbol/hint name.
        let batches: Vec<RecordBatch> = if let Some(n) = node {
            let safe_fp = n.file_path.replace('\'', "''");
            let filter = format!(
                "(path = '{safe_fp}' OR path LIKE '%/{safe_fp}' OR path LIKE '%\\\\{safe_fp}') \
                 AND start_line = {}",
                n.start_line
            );
            tbl.query()
                .only_if(filter)
                .execute()
                .await
                .map_err(|e| SempkgError::LanceError(e.to_string()))?
                .try_collect()
                .await
                .map_err(|e| SempkgError::LanceError(e.to_string()))?
        } else {
            // No SQLite DB available — fall back to FTS on the symbol/hint name
            // and do an exact client-side match.
            tbl.query()
                .full_text_search(FullTextSearchQuery::new(hint.to_string()))
                .limit(50)
                .execute()
                .await
                .map_err(|e| SempkgError::LanceError(e.to_string()))?
                .try_collect()
                .await
                .map_err(|e| SempkgError::LanceError(e.to_string()))?
        };

        // Extract the first matching row.
        for batch in &batches {
            let syms = batch
                .column_by_name("symbol")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());
            let paths = batch
                .column_by_name("path")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());
            let kinds = batch
                .column_by_name("kind")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());
            let sigs = batch
                .column_by_name("signature")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());
            let contents = batch
                .column_by_name("content")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());
            let start_lines = batch
                .column_by_name("start_line")
                .and_then(|c| c.as_any().downcast_ref::<UInt32Array>());
            let end_lines = batch
                .column_by_name("end_line")
                .and_then(|c| c.as_any().downcast_ref::<UInt32Array>());

            if let (Some(p), Some(c)) = (paths, contents) {
                for i in 0..batch.num_rows() {
                    // When using FTS fallback, require exact symbol name match.
                    if node.is_none() {
                        let sym_val = syms.map_or("", |a| a.value(i));
                        if sym_val != hint {
                            continue;
                        }
                    }

                    let row_start = start_lines.map_or(0, |a| a.value(i));
                    let row_end = end_lines.map_or(0, |a| a.value(i));

                    // Determine the exact line range to extract.
                    let (exact_start, exact_end) = if let Some(n) = node {
                        (n.start_line, n.end_line)
                    } else {
                        (row_start, row_end)
                    };

                    // Slice the stored content to the precise codegraph range
                    // in case the chunk is wider than the symbol.
                    let raw_content = c.value(i);
                    let content =
                        slice_content_to_range(raw_content, row_start, exact_start, exact_end);

                    let sym_name = node
                        .map(|n| n.name.clone())
                        .or_else(|| syms.map(|a| a.value(i).to_string()))
                        .unwrap_or_default();
                    let kind = node
                        .map(|n| n.kind.clone())
                        .or_else(|| kinds.map(|a| a.value(i).to_string()))
                        .unwrap_or_default();
                    let signature = node
                        .as_ref()
                        .and_then(|n| n.signature.clone())
                        .or_else(|| sigs.map(|a| a.value(i).to_string()))
                        .unwrap_or_default();

                    return Ok(Some(SymbolSource {
                        path: p.value(i).to_string(),
                        symbol: sym_name,
                        kind,
                        signature,
                        content,
                        start_line: exact_start,
                        end_line: exact_end,
                    }));
                }
            }
        }
        Ok::<Option<SymbolSource>, SempkgError>(None)
    })?;

    Ok(result)
}

/// Slice `content` (which starts at `chunk_start` line) to the range
/// `[exact_start, exact_end]` (both 1-based, inclusive).
/// Returns the full content unchanged when the chunk already matches or when
/// line numbers are zero/unavailable.
fn slice_content_to_range(
    content: &str,
    chunk_start: u32,
    exact_start: u32,
    exact_end: u32,
) -> String {
    if chunk_start == 0 || exact_start == 0 || exact_end == 0 {
        return content.to_string();
    }
    if chunk_start == exact_start {
        // The chunk starts exactly at the symbol — just drop trailing lines.
        let keep = (exact_end - exact_start + 1) as usize;
        let result: Vec<&str> = content.lines().take(keep).collect();
        return result.join("\n");
    }
    // The chunk starts before the symbol (larger chunk) — extract the sub-range.
    let skip = (exact_start.saturating_sub(chunk_start)) as usize;
    let keep = (exact_end - exact_start + 1) as usize;
    let result: Vec<&str> = content.lines().skip(skip).take(keep).collect();
    if result.is_empty() {
        content.to_string() // safety: return full content if arithmetic went wrong
    } else {
        result.join("\n")
    }
}

/// LanceDB-only fallback for `fetch_symbol_at_location` when `codegraph.db`
/// is not present.  Preserves the original tightest-range heuristic.
fn fetch_at_location_lance_fallback(
    code_dir: &Path,
    file: &str,
    line: u32,
) -> crate::error::Result<Option<SymbolSource>> {
    let safe_file = file.replace('\'', "''");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(SempkgError::Io)?;

    let result = rt.block_on(async {
        let db = lancedb::connect(code_dir.to_str().unwrap_or("."))
            .execute()
            .await
            .map_err(|e| SempkgError::LanceError(e.to_string()))?;

        let tbl = db
            .open_table("code")
            .execute()
            .await
            .map_err(|e| SempkgError::LanceError(e.to_string()))?;

        let filter = format!(
            "(path = '{safe_file}' OR path LIKE '%/{safe_file}' OR path LIKE '%\\\\{safe_file}') \
             AND start_line <= {line} AND end_line >= {line}"
        );

        let batches: Vec<RecordBatch> = tbl
            .query()
            .only_if(filter)
            .execute()
            .await
            .map_err(|e| SempkgError::LanceError(e.to_string()))?
            .try_collect()
            .await
            .map_err(|e| SempkgError::LanceError(e.to_string()))?;

        let mut best: Option<SymbolSource> = None;

        for batch in &batches {
            let syms = batch
                .column_by_name("symbol")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());
            let paths = batch
                .column_by_name("path")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());
            let kinds = batch
                .column_by_name("kind")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());
            let sigs = batch
                .column_by_name("signature")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());
            let contents = batch
                .column_by_name("content")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());
            let start_lines = batch
                .column_by_name("start_line")
                .and_then(|c| c.as_any().downcast_ref::<UInt32Array>());
            let end_lines = batch
                .column_by_name("end_line")
                .and_then(|c| c.as_any().downcast_ref::<UInt32Array>());

            if let (Some(s), Some(p), Some(c)) = (syms, paths, contents) {
                for i in 0..batch.num_rows() {
                    let start = start_lines.map_or(0, |a| a.value(i));
                    let end = end_lines.map_or(0, |a| a.value(i));

                    let candidate = SymbolSource {
                        path: p.value(i).to_string(),
                        symbol: s.value(i).to_string(),
                        kind: kinds.map_or("", |a| a.value(i)).to_string(),
                        signature: sigs.map_or("", |a| a.value(i)).to_string(),
                        content: c.value(i).to_string(),
                        start_line: start,
                        end_line: end,
                    };

                    let span = end.saturating_sub(start);
                    let is_better = match &best {
                        None => true,
                        Some(prev) => span < prev.end_line.saturating_sub(prev.start_line),
                    };
                    if is_better {
                        best = Some(candidate);
                    }
                }
            }
        }

        Ok::<Option<SymbolSource>, SempkgError>(best)
    })?;

    Ok(result)
}

/// Fetch raw documentation content for `file` from the docs LanceDB table,
/// optionally narrowed to a `[start_line, end_line]` range. This backs the
/// `read_docs` MCP tool: an agent finds a relevant location with `search_docs`,
/// then reads the surrounding raw content here without the snippet truncation
/// applied by search.
///
/// Resolution notes:
/// - Doc paths are stored either plain (`api.md`) by the local indexer or with
///   a per-chunk suffix (`api.md#0`) by `sembundle`; both are matched.
/// - When `start_line`/`end_line` are `None` the entire file is returned. When
///   only one bound is given the other is treated as open-ended.
/// - With a range, content is resolved to **whole lines**: each chunk's stored
///   text is the exact source slice, so it projects onto absolute line numbers
///   and only the lines inside the request are returned (never a partial line,
///   and never the chunk's overshoot beyond the request). Dropped blank lines
///   between chunks are reconstructed so the slice reads contiguously.
/// - When the table predates line metadata the range is ignored and every chunk
///   is returned with `line_meta_missing = true`.
///
/// Returns `Ok(None)` when no chunk matches `file` (or, for a range, when no
/// line falls inside it).
pub fn fetch_doc_lines(
    lance_dir: &Path,
    file: &str,
    start_line: Option<u32>,
    end_line: Option<u32>,
) -> crate::error::Result<Option<DocSlice>> {
    if !lance_dir.is_dir() {
        return Err(SempkgError::NoLanceIndex(
            lance_dir.to_string_lossy().to_string(),
        ));
    }

    let safe_file = file.replace('\'', "''");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(SempkgError::Io)?;

    let result = rt.block_on(async {
        let db = lancedb::connect(lance_dir.to_str().unwrap_or("."))
            .execute()
            .await
            .map_err(|e| SempkgError::LanceError(e.to_string()))?;

        let tbl = db
            .open_table("docs")
            .execute()
            .await
            .map_err(|e| SempkgError::LanceError(e.to_string()))?;

        // Match the file with or without a per-chunk `#N` suffix, and whether or
        // not it is stored with a leading directory prefix.
        let filter = format!(
            "(path = '{safe_file}' OR path LIKE '{safe_file}#%' \
             OR path LIKE '%/{safe_file}' OR path LIKE '%/{safe_file}#%')"
        );

        let batches: Vec<RecordBatch> = tbl
            .query()
            .only_if(filter)
            .execute()
            .await
            .map_err(|e| SempkgError::LanceError(e.to_string()))?
            .try_collect()
            .await
            .map_err(|e| SempkgError::LanceError(e.to_string()))?;

        // (chunk_index, start_line, end_line, content) per matched row.
        let mut rows: Vec<(u32, u32, u32, String)> = Vec::new();
        let mut resolved_path: Option<String> = None;
        let mut has_line_meta = false;

        for batch in &batches {
            let paths = batch
                .column_by_name("path")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());
            let contents = batch
                .column_by_name("content")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());
            let start_lines = batch
                .column_by_name("start_line")
                .and_then(|c| c.as_any().downcast_ref::<UInt32Array>());
            let end_lines = batch
                .column_by_name("end_line")
                .and_then(|c| c.as_any().downcast_ref::<UInt32Array>());

            if start_lines.is_some() {
                has_line_meta = true;
            }

            if let (Some(p), Some(c)) = (paths, contents) {
                for i in 0..batch.num_rows() {
                    let raw_path = p.value(i);
                    // The display path drops the `#N` chunk suffix.
                    let (clean_path, chunk_idx) = match raw_path.split_once('#') {
                        Some((f, idx)) => (f.to_string(), idx.parse::<u32>().unwrap_or(0)),
                        None => (raw_path.to_string(), i as u32),
                    };
                    if resolved_path.is_none() {
                        resolved_path = Some(clean_path);
                    }
                    rows.push((
                        chunk_idx,
                        start_lines.map_or(0, |a| a.value(i)),
                        end_lines.map_or(0, |a| a.value(i)),
                        c.value(i).to_string(),
                    ));
                }
            }
        }

        if rows.is_empty() {
            return Ok::<Option<DocSlice>, SempkgError>(None);
        }

        // Order chunks by source position: line number when available, else by
        // the stored `#N` chunk index.
        if has_line_meta {
            rows.sort_by_key(|r| (r.1, r.0));
        } else {
            rows.sort_by_key(|r| r.0);
        }

        // Line-accurate path: when per-chunk line numbers are present and the
        // caller asked for a range, resolve to whole lines within `[lo, hi]`
        // rather than returning entire chunks (which overshoot the request and,
        // for older byte-windowed bundles, can begin or end mid-line).
        //
        // Chunk text is stored as the exact source slice, so the i-th line of a
        // chunk beginning at source line `s` is file line `s + i`. We project
        // every overlapping chunk onto its absolute line numbers, keep the lines
        // inside the request, and reconstruct the gaps (dropped blank lines
        // between chunks) so the result reads contiguously.
        let range_requested = start_line.is_some() || end_line.is_some();
        if has_line_meta && range_requested {
            let lo = start_line.unwrap_or(0);
            let hi = end_line.unwrap_or(u32::MAX);

            let mut line_map: BTreeMap<u32, &str> = BTreeMap::new();
            let mut contributing: BTreeSet<usize> = BTreeSet::new();
            for (idx, (_, s, _, content)) in rows.iter().enumerate() {
                if *s == 0 {
                    continue; // unknown position; cannot line-resolve
                }
                for (i, line) in content.split('\n').enumerate() {
                    let ln = s + i as u32;
                    if ln >= lo && ln <= hi {
                        line_map.insert(ln, line);
                        contributing.insert(idx);
                    }
                }
            }

            if line_map.is_empty() {
                return Ok(None);
            }

            let covered_start = *line_map.keys().next().unwrap();
            let covered_end = *line_map.keys().next_back().unwrap();
            let content = (covered_start..=covered_end)
                .map(|ln| line_map.get(&ln).copied().unwrap_or(""))
                .collect::<Vec<_>>()
                .join("\n");

            return Ok(Some(DocSlice {
                path: resolved_path.unwrap_or_else(|| file.to_string()),
                content,
                start_line: covered_start,
                end_line: covered_end,
                chunk_count: contributing.len(),
                line_meta_missing: false,
            }));
        }

        if rows.is_empty() {
            return Ok(None);
        }

        // Whole-file (or line-metadata-less) path: return every matched chunk.
        let chunk_count = rows.len();
        let covered_start = rows
            .iter()
            .map(|r| r.1)
            .filter(|&s| s > 0)
            .min()
            .unwrap_or(0);
        let covered_end = rows.iter().map(|r| r.2).max().unwrap_or(0);
        let content = rows
            .into_iter()
            .map(|r| r.3)
            .collect::<Vec<_>>()
            .join("\n\n");

        Ok(Some(DocSlice {
            path: resolved_path.unwrap_or_else(|| file.to_string()),
            content,
            start_line: covered_start,
            end_line: covered_end,
            chunk_count,
            line_meta_missing: !has_line_meta,
        }))
    })?;

    Ok(result)
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
            if let Some(sym) = &r.symbol {
                let kind = r.kind.as_deref().unwrap_or("symbol");
                let sig = r.signature.as_deref().unwrap_or("");
                let sig_part = if sig.is_empty() {
                    String::new()
                } else {
                    format!("\n_{sig}_")
                };
                format!(
                    "**{sym}** ({kind}) @ {loc}{sig_part}\n\n```\n{}\n```",
                    r.snippet
                )
            } else {
                format!("**{}**\n\n{}", loc, r.snippet)
            }
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
            '{' => {
                depth += 1;
                current.push(ch);
            }
            '}' => {
                depth = depth.saturating_sub(1);
                current.push(ch);
            }
            ',' if depth == 0 => {
                let t = current.trim().to_string();
                if !t.is_empty() {
                    tokens.push(t);
                }
                current.clear();
            }
            _ => {
                current.push(ch);
            }
        }
    }
    let t = current.trim().to_string();
    if !t.is_empty() {
        tokens.push(t);
    }

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
            Field::new("path", DataType::Utf8, false),
            Field::new("content", DataType::Utf8, false),
            Field::new("start_line", DataType::UInt32, false),
            Field::new("end_line", DataType::UInt32, false),
            Field::new("start_byte", DataType::UInt32, false),
            Field::new("end_byte", DataType::UInt32, false),
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

        let batches = vec![batch];

        let db = lancedb::connect(lance_out.to_str().unwrap_or("."))
            .execute()
            .await
            .map_err(|e| SempkgError::LanceError(e.to_string()))?;

        // Drop existing table if present so re-indexing works.
        let _ = db.drop_table("docs", &[]).await;

        let tbl = db
            .create_table("docs", batches)
            .execute()
            .await
            .map_err(|e| SempkgError::LanceError(e.to_string()))?;

        let _ = tbl
            .create_index(
                &["content"],
                lancedb::index::Index::FTS(lancedb::index::scalar::FtsIndexBuilder::default()),
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
        serde_json::to_vec_pretty(&meta).map_err(SempkgError::Json)?,
    )?;

    eprintln!("[sempkg] lance: indexed {doc_count} documents, {chunk_count} chunks.");

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
    text[..boundary].bytes().filter(|&b| b == b'\n').count() as u32 + 1
}

/// Push the exact source slice `text[start..end]` as a chunk.
///
/// Storing the verbatim slice (rather than a re-joined copy) is what makes line
/// resolution exact downstream: splitting the stored text on `'\n'` maps line
/// `i` back to source line `start_line + i`. A trailing newline is trimmed so
/// the slice ends on real content and `end_line` is the line of the last byte.
fn push_chunk(text: &str, start: usize, mut end: usize, chunks: &mut Vec<ChunkInfo>) {
    let bytes = text.as_bytes();
    while end > start && (bytes[end - 1] == b'\n' || bytes[end - 1] == b'\r') {
        end -= 1;
    }
    if end <= start {
        return;
    }
    chunks.push(ChunkInfo {
        text: text[start..end].to_string(),
        start_line: byte_to_line(text, start),
        end_line: byte_to_line(text, end - 1),
        start_byte: start as u32,
        end_byte: end as u32,
    });
}

/// Split the oversized paragraph at `[start, end)` into chunks that never begin
/// or end mid-line. Whole lines are accumulated until the next line would
/// overflow `max_chars`; a single line longer than `max_chars` is hard-split on
/// char boundaries as a last resort.
fn push_oversized_on_lines(
    text: &str,
    start: usize,
    end: usize,
    max_chars: usize,
    chunks: &mut Vec<ChunkInfo>,
) {
    // Absolute byte offsets where each line begins within [start, end).
    let bytes = text.as_bytes();
    let mut line_starts = vec![start];
    for (offset, &b) in bytes[start..end].iter().enumerate() {
        if b == b'\n' && start + offset + 1 < end {
            line_starts.push(start + offset + 1);
        }
    }
    line_starts.push(end); // sentinel: one past the last line

    let mut win_start = start;
    for w in 1..line_starts.len() {
        let line_lo = line_starts[w - 1];
        let line_hi = line_starts[w];
        // Adding this line would overflow a non-empty window: flush whole lines
        // accumulated so far first.
        if line_hi - win_start > max_chars && line_lo > win_start {
            push_chunk(text, win_start, line_lo, chunks);
            win_start = line_lo;
        }
        // A single line longer than the budget: hard-split it on char
        // boundaries (the one unavoidable mid-line case).
        if line_hi - line_lo > max_chars {
            let mut off = line_lo;
            while off < line_hi {
                let mut stop = (off + max_chars).min(line_hi);
                while stop > off && !text.is_char_boundary(stop) {
                    stop -= 1;
                }
                if stop == off {
                    stop = line_hi;
                }
                push_chunk(text, off, stop, chunks);
                off = stop;
            }
            win_start = line_hi;
        }
    }
    if end > win_start {
        push_chunk(text, win_start, end, chunks);
    }
}

fn chunk_text(text: &str, max_chars: usize) -> Vec<ChunkInfo> {
    // Collect paragraphs (split on \n\n) as byte ranges in `text`. `str::split`
    // returns subslices of the original, so pointer arithmetic gives us each
    // paragraph's byte offset.
    let mut paras: Vec<(usize, usize)> = Vec::new();
    for raw in text.split("\n\n") {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let raw_start = raw.as_ptr() as usize - text.as_ptr() as usize;
        let trim_lead = raw.len() - raw.trim_start().len();
        let start = raw_start + trim_lead;
        paras.push((start, start + trimmed.len()));
    }

    let mut chunks: Vec<ChunkInfo> = Vec::new();
    // Accumulate consecutive paragraphs as a single source span so the stored
    // chunk is the exact substring `text[cur_start..cur_end]`.
    let mut cur_start: Option<usize> = None;
    let mut cur_end = 0usize;

    for &(p_start, p_end) in &paras {
        // Measure the prospective chunk on its original byte span — that is the
        // slice we will actually store.
        let fits = match cur_start {
            None => p_end - p_start <= max_chars,
            Some(s) => p_end - s <= max_chars,
        };

        if fits {
            cur_start.get_or_insert(p_start);
            cur_end = p_end;
            continue;
        }

        // Flush what we have, then restart with this paragraph.
        if let Some(s) = cur_start.take() {
            push_chunk(text, s, cur_end, &mut chunks);
        }
        if p_end - p_start <= max_chars {
            cur_start = Some(p_start);
            cur_end = p_end;
        } else {
            push_oversized_on_lines(text, p_start, p_end, max_chars, &mut chunks);
            cur_start = None;
        }
    }
    if let Some(s) = cur_start {
        push_chunk(text, s, cur_end, &mut chunks);
    }

    if chunks.is_empty() && !text.is_empty() {
        // No paragraph breaks at all: fall back to line-aware windowing over the
        // whole text so we still avoid mid-line cuts where possible.
        push_oversized_on_lines(text, 0, text.len(), max_chars, &mut chunks);
    }
    chunks
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Row spec for a synthetic docs table: (path, content, start_line, end_line).
    type DocRow = (&'static str, &'static str, u32, u32);

    /// Write a `docs` LanceDB table at `lance_dir`. When `with_lines` is false
    /// the line-range columns are omitted, emulating an older bundle.
    fn build_docs_table(lance_dir: &Path, rows: &[DocRow], with_lines: bool) {
        std::fs::create_dir_all(lance_dir).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut fields = vec![
                Field::new("path", DataType::Utf8, false),
                Field::new("content", DataType::Utf8, false),
            ];
            if with_lines {
                fields.push(Field::new("start_line", DataType::UInt32, false));
                fields.push(Field::new("end_line", DataType::UInt32, false));
            }
            let schema = Arc::new(Schema::new(fields));

            let mut columns: Vec<Arc<dyn Array>> = vec![
                Arc::new(StringArray::from(
                    rows.iter().map(|r| r.0).collect::<Vec<_>>(),
                )),
                Arc::new(StringArray::from(
                    rows.iter().map(|r| r.1).collect::<Vec<_>>(),
                )),
            ];
            if with_lines {
                columns.push(Arc::new(UInt32Array::from(
                    rows.iter().map(|r| r.2).collect::<Vec<_>>(),
                )));
                columns.push(Arc::new(UInt32Array::from(
                    rows.iter().map(|r| r.3).collect::<Vec<_>>(),
                )));
            }

            let batch = RecordBatch::try_new(schema, columns).unwrap();
            let db = lancedb::connect(lance_dir.to_str().unwrap())
                .execute()
                .await
                .unwrap();
            let _ = db.drop_table("docs", &[]).await;
            db.create_table("docs", vec![batch])
                .execute()
                .await
                .unwrap();
        });
    }

    #[test]
    fn read_docs_whole_file_concatenates_chunks_in_order() {
        let dir = tempdir().unwrap();
        let lance_dir = dir.path();
        // Two chunks of guide.md (stored with sembundle-style `#N` suffix) plus
        // an unrelated file. Insert out of order to prove ordering by line.
        build_docs_table(
            lance_dir,
            &[
                ("other.md#0", "unrelated content", 1, 2),
                ("guide.md#1", "second chunk body", 5, 7),
                ("guide.md#0", "first chunk body", 1, 3),
            ],
            true,
        );

        let slice = fetch_doc_lines(lance_dir, "guide.md", None, None)
            .unwrap()
            .expect("guide.md should be found");

        assert_eq!(slice.chunk_count, 2);
        assert_eq!(slice.start_line, 1);
        assert_eq!(slice.end_line, 7);
        assert!(!slice.line_meta_missing);
        // Ordered by line: first chunk precedes the second; no other.md leakage.
        assert_eq!(slice.content, "first chunk body\n\nsecond chunk body");
        assert!(!slice.content.contains("unrelated"));
    }

    #[test]
    fn read_docs_line_range_returns_only_requested_lines() {
        let dir = tempdir().unwrap();
        let lance_dir = dir.path();
        // Two chunks whose stored text is the exact source slice: each line of a
        // chunk maps to `start_line + i`. Lines 4 falls in the gap between them.
        build_docs_table(
            lance_dir,
            &[
                ("guide.md#0", "line1\nline2\nline3", 1, 3),
                ("guide.md#1", "line5\nline6\nline7", 5, 7),
            ],
            true,
        );

        // A sub-chunk range returns only those whole lines, not the whole chunk.
        let slice = fetch_doc_lines(lance_dir, "guide.md", Some(6), Some(7))
            .unwrap()
            .expect("guide.md should be found");
        assert_eq!(slice.content, "line6\nline7");
        assert_eq!(slice.start_line, 6);
        assert_eq!(slice.end_line, 7);
        assert_eq!(slice.chunk_count, 1);

        // A range spanning both chunks keeps only the requested lines from each
        // and reconstructs the dropped blank line (4) between them.
        let slice = fetch_doc_lines(lance_dir, "guide.md", Some(2), Some(6))
            .unwrap()
            .unwrap();
        assert_eq!(slice.content, "line2\nline3\n\nline5\nline6");
        assert_eq!(slice.start_line, 2);
        assert_eq!(slice.end_line, 6);
        assert_eq!(slice.chunk_count, 2);

        // An open-ended lower bound (lines 6..) keeps lines 6 and 7.
        let slice = fetch_doc_lines(lance_dir, "guide.md", Some(6), None)
            .unwrap()
            .unwrap();
        assert_eq!(slice.content, "line6\nline7");
    }

    #[test]
    fn read_docs_unknown_file_returns_none() {
        let dir = tempdir().unwrap();
        build_docs_table(dir.path(), &[("guide.md#0", "body", 1, 3)], true);
        let slice = fetch_doc_lines(dir.path(), "missing.md", None, None).unwrap();
        assert!(slice.is_none());
    }

    #[test]
    fn read_docs_without_line_metadata_returns_all_chunks() {
        let dir = tempdir().unwrap();
        let lance_dir = dir.path();
        // Older bundle: no line columns at all.
        build_docs_table(
            lance_dir,
            &[("guide.md#0", "alpha", 0, 0), ("guide.md#1", "beta", 0, 0)],
            false,
        );

        // Even with a requested range, the whole file comes back and the caller
        // is told line metadata was unavailable.
        let slice = fetch_doc_lines(lance_dir, "guide.md", Some(100), Some(200))
            .unwrap()
            .unwrap();
        assert!(slice.line_meta_missing);
        assert_eq!(slice.chunk_count, 2);
        assert!(slice.content.contains("alpha") && slice.content.contains("beta"));
    }

    #[test]
    fn read_docs_end_to_end_via_cli_update() {
        // Exercises the real indexing path (cli_update writes line metadata) and
        // the read_docs retrieval against it.
        let project = tempdir().unwrap();
        let docs_sub = project.path().join("docs");
        std::fs::create_dir_all(&docs_sub).unwrap();
        let md = "# Title\n\nAlpha paragraph about widgets.\n\nBeta paragraph about gadgets.\n";
        std::fs::write(docs_sub.join("guide.md"), md).unwrap();

        let lance_dir = cli_update(project.path(), "docs", "**/*.md").unwrap();

        let slice = fetch_doc_lines(&lance_dir, "docs/guide.md", None, None)
            .unwrap()
            .expect("indexed doc should be found");
        assert_eq!(slice.start_line, 1);
        assert!(slice.end_line >= 5, "end_line was {}", slice.end_line);
        assert!(!slice.line_meta_missing);
        assert!(slice.content.contains("Alpha"));
        assert!(slice.content.contains("Beta"));

        assert!(fetch_doc_lines(&lance_dir, "nope.md", None, None)
            .unwrap()
            .is_none());
    }

    #[test]
    fn read_docs_line_range_resolves_whole_lines_via_cli_update() {
        // Drive the real chunker: a single multi-line paragraph is stored as one
        // exact slice, so read_docs can resolve a request down to whole lines.
        let project = tempdir().unwrap();
        let docs_sub = project.path().join("docs");
        std::fs::create_dir_all(&docs_sub).unwrap();
        let md = "alpha one\nbeta two\ngamma three\ndelta four\nepsilon five\nzeta six\n";
        std::fs::write(docs_sub.join("nums.md"), md).unwrap();

        let lance_dir = cli_update(project.path(), "docs", "**/*.md").unwrap();

        let slice = fetch_doc_lines(&lance_dir, "docs/nums.md", Some(3), Some(4))
            .unwrap()
            .expect("indexed doc should be found");
        assert_eq!(slice.start_line, 3);
        assert_eq!(slice.end_line, 4);
        assert_eq!(slice.content, "gamma three\ndelta four");
        assert!(!slice.line_meta_missing);
    }
}

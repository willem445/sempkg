//! Native Rust reader **and writer** for a CodeGraph `codegraph.db` (SQLite,
//! schema v4).
//!
//! This crate owns both the read path for a bundle's graph database — replacing
//! the former shell-out to the `codegraph` CLI for query operations (issue #78,
//! Phase 1) — and the native write path that produces such a database from
//! source without any external tooling (issue #78, Phase 2a). Sharing one crate
//! keeps a single definition of the schema and record types across both sides.
//!
//! ## Reader ([`GraphDb`])
//!
//! Read-only, scoped to a single database file: [`GraphDb::query`],
//! [`GraphDb::callers`]/[`GraphDb::callees`], [`GraphDb::impact`],
//! [`GraphDb::context`], [`GraphDb::status`], [`GraphDb::file_paths`].
//!
//! ## Writer / indexer ([`index_roots`], [`sync`], [`GraphWriter`], [`extract`])
//!
//! [`index_roots`] is the entry point: it walks one or more source roots, parses
//! every supported file **in parallel** (rayon) with tree-sitter, extracts
//! definition nodes and structural `contains` edges (see [`parse`]), then
//! **resolves** every call/reference/import/instantiation site against a global
//! symbol table (see `resolve`, Phase 2b) into `calls`/`references`/`imports`/
//! `instantiates` edges, and writes one schema-v4 database through a
//! single-writer [`GraphWriter`] in one transaction. The result is
//! byte-compatible with a CodeGraph-built DB — the reader above opens it
//! unchanged.
//!
//! Resolution is two-pass and deterministic: pass 1 extracts definitions plus
//! per-file reference *sites* ([`FileExtract`] with qualified names and stable
//! [`node_id`]s); pass 2 resolves each site by name/qualified-name against the
//! symbol table with scope-precedence heuristics (same-file > import-target >
//! unique-global), preferring precision over recall for `calls` edges. See the
//! `resolve` module docs.
//!
//! ## Incremental sync ([`sync`])
//!
//! [`sync`] brings an existing database up to date with the source tree,
//! re-parsing only files whose `files.content_hash` changed (plus added/deleted
//! files) and re-resolving the edges the delta invalidates — the reverse
//! dependency blast radius. The result is canonically equal to a fresh
//! [`index_roots`]. See the [`index`] module docs for the invalidation rule.
//!
//! ### Multi-root indexing and file-path representation (issue #79)
//!
//! [`index_roots`] takes **multiple** source roots and writes **one** database —
//! the proper fix for #79, where multiple `-s`/`--source-dir` roots used to make
//! the last root silently overwrite the rest. Stored `file_path`s are chosen to
//! be unambiguous across roots yet consistent with how consumers resolve paths
//! back to disk: a single root keeps CodeGraph-relative paths (`python/main.py`);
//! multiple roots are namespaced by the shortest distinguishing suffix of each
//! root path. [`resolve_stored_path`] maps a stored path back to
//! `(root, relative)` for the code-index/`read_code` cutover. See the [`index`]
//! module docs for the full rule.
//!
//! ### `files.content_hash`
//!
//! [`content_hash`] (SHA-256 of the file bytes) is written to every `files` row
//! and is the anchor for Phase 2b incremental sync: an unchanged file hashes the
//! same and can be skipped on re-index.
//!
//! ## CodeGraph 0.9.7 quirks the reader must tolerate
//!
//! Two schema-v4 tables are present but **always contain 0 rows** in a DB
//! produced by CodeGraph 0.9.7, for *any* input — verified empirically and
//! against the tool's source (see `tests/fixtures/README.md`):
//!
//! - **`unresolved_refs`** is drained after each resolution batch during
//!   indexing, so it is empty by the time indexing finishes. It is transient
//!   scratch space, never durable data.
//! - **`project_metadata`** is never written (`setMetadata` is defined but
//!   never called), so [`GraphDb::status`] derives its counts from the
//!   `files`/`nodes`/`edges` tables and never depends on `project_metadata`.
//!
//! A reader may still `SELECT` from these tables (they exist), but must treat
//! 0 rows as the normal, expected case.

use std::collections::HashMap;
use std::path::Path;

use rusqlite::{Connection, OpenFlags, OptionalExtension, Row};
use thiserror::Error;

pub mod index;
pub mod model;
pub mod parse;
pub(crate) mod resolve;
pub mod writer;

pub use index::{
    index_roots, namespaces_for_roots, resolve_stored_path, sync, IndexOptions, IndexStats,
};
pub use model::{content_hash, node_id, EdgeRecord, FileRecord, Language, NodeRecord};
pub use parse::{extract, FileExtract};
pub use writer::GraphWriter;

/// The maximum `schema_versions.version` this reader understands.
///
/// CodeGraph 0.9.7 produces schema v4. Bundles built with a newer CodeGraph
/// that bumps the schema are rejected by [`GraphDb::open`] with an actionable
/// error rather than being silently mis-read.
pub const SUPPORTED_SCHEMA_VERSION: i64 = 4;

/// Errors returned by the graph reader.
#[derive(Debug, Error)]
pub enum Error {
    #[error("cannot open codegraph.db at '{path}': {source}")]
    Open {
        path: String,
        #[source]
        source: rusqlite::Error,
    },

    #[error(
        "unsupported CodeGraph schema version {found} in '{path}' \
         (this build understands up to schema v{supported}). \
         The bundle was produced by a newer CodeGraph than this reader; \
         upgrade semgraph or rebuild the bundle with CodeGraph 0.9.7."
    )]
    UnsupportedSchema {
        path: String,
        found: i64,
        supported: i64,
    },

    #[error("'{path}' is not a valid codegraph.db: {detail}")]
    Invalid { path: String, detail: String },

    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Columns selected for every [`GraphNode`], in the order [`row_to_node`] reads
/// them. Kept as a single constant so all node queries stay in sync.
const NODE_COLUMNS: &str = "\
    n.id, n.kind, n.name, n.qualified_name, n.file_path, n.language, \
    n.start_line, n.end_line, n.signature, n.docstring, n.is_async";

/// A node read from the `nodes` table.
///
/// This is a **read projection**: it carries the columns the query surfaces
/// consume today, not the full schema-v4 node. The remaining columns
/// (`start_column`/`end_column`, `visibility`, the `is_exported`/`is_static`/
/// `is_abstract` flags, `decorators`, `type_parameters`, `updated_at`) — and a
/// companion `EdgeRecord` — are added alongside the Phase 2a writer, which
/// needs to round-trip the whole schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphNode {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub qualified_name: String,
    pub file_path: String,
    pub language: String,
    pub start_line: u32,
    pub end_line: u32,
    pub signature: Option<String>,
    pub docstring: Option<String>,
    pub is_async: bool,
}

/// A node reached across a `calls` edge, carrying the call-site line when the
/// edge records one (`edges.line`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallEdge {
    pub node: GraphNode,
    /// Line of the call site (from `edges.line`); `None` when the edge has no
    /// recorded line.
    pub line: Option<u32>,
}

/// Summary counts for a graph database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphStatus {
    pub schema_version: i64,
    pub file_count: u64,
    pub node_count: u64,
    pub edge_count: u64,
}

/// A read-only handle to a CodeGraph `codegraph.db`.
#[derive(Debug)]
pub struct GraphDb {
    conn: Connection,
    schema_version: i64,
}

impl GraphDb {
    /// Open `db_path` read-only and validate its schema version.
    ///
    /// Returns [`Error::UnsupportedSchema`] when the database declares a schema
    /// newer than [`SUPPORTED_SCHEMA_VERSION`].
    ///
    /// The database is opened `immutable=1` (via a `file:` URI): bundle graphs
    /// ship as read-only, checkpointed artifacts, so promising SQLite the file
    /// will not change lets it read a WAL-mode DB without creating `-wal`/`-shm`
    /// sidecars — which matters on genuinely read-only installs where creating
    /// them would fail. (The `.gitignore` rule for the sidecars is thus only a
    /// belt-and-suspenders backstop.)
    pub fn open(db_path: &Path) -> Result<Self> {
        let uri = to_sqlite_uri(db_path);
        let conn = Connection::open_with_flags(
            &uri,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_URI,
        )
        .map_err(|source| Error::Open {
            path: db_path.display().to_string(),
            source,
        })?;

        let schema_version = read_schema_version(&conn, db_path)?;
        if schema_version > SUPPORTED_SCHEMA_VERSION {
            return Err(Error::UnsupportedSchema {
                path: db_path.display().to_string(),
                found: schema_version,
                supported: SUPPORTED_SCHEMA_VERSION,
            });
        }

        Ok(GraphDb {
            conn,
            schema_version,
        })
    }

    /// The schema version declared by the opened database.
    pub fn schema_version(&self) -> i64 {
        self.schema_version
    }

    /// Search symbols by name/qualified-name.
    ///
    /// Uses the `nodes_fts` FTS5 index first (token match, ranked by relevance).
    /// When the FTS query is unusable (syntactically rejected input) or yields
    /// no rows, falls back to a case-insensitive substring `LIKE` — this both
    /// tolerates arbitrary user input and catches partial-token matches FTS
    /// cannot (e.g. `ircle` → `Circle`). `kind` filters to a single node kind.
    pub fn query(&self, search: &str, kind: Option<&str>, limit: usize) -> Result<Vec<GraphNode>> {
        // Empty/whitespace-only input has no meaningful match. Short-circuit to
        // an empty result rather than letting the `LIKE '%%'` fallback match
        // every node in the graph.
        if search.trim().is_empty() {
            return Ok(Vec::new());
        }
        let fts = self.query_fts(search, kind, limit).unwrap_or_default();
        if !fts.is_empty() {
            return Ok(fts);
        }
        self.query_like(search, kind, limit)
    }

    fn query_fts(&self, search: &str, kind: Option<&str>, limit: usize) -> Result<Vec<GraphNode>> {
        let mut sql = format!(
            "SELECT {NODE_COLUMNS} \
             FROM nodes_fts \
             JOIN nodes n ON n.rowid = nodes_fts.rowid \
             WHERE nodes_fts MATCH ?1"
        );
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(fts_match_phrase(search))];
        if let Some(k) = kind {
            sql.push_str(" AND n.kind = ?2");
            params.push(Box::new(k.to_string()));
        }
        sql.push_str(" ORDER BY nodes_fts.rank");
        sql.push_str(&format!(" LIMIT {}", limit.max(1)));
        self.collect_nodes(&sql, params)
    }

    fn query_like(&self, search: &str, kind: Option<&str>, limit: usize) -> Result<Vec<GraphNode>> {
        let pattern = format!("%{}%", escape_like(search));
        let mut sql = format!(
            "SELECT {NODE_COLUMNS} FROM nodes n \
             WHERE (n.name LIKE ?1 ESCAPE '\\' OR n.qualified_name LIKE ?1 ESCAPE '\\')"
        );
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(pattern)];
        if let Some(k) = kind {
            sql.push_str(" AND n.kind = ?2");
            params.push(Box::new(k.to_string()));
        }
        // Prefer the tightest (smallest range) definitions first for stable output.
        sql.push_str(" ORDER BY (n.end_line - n.start_line) ASC, n.qualified_name ASC");
        sql.push_str(&format!(" LIMIT {}", limit.max(1)));
        self.collect_nodes(&sql, params)
    }

    /// Find callers of `symbol`: nodes on the source side of a `calls` edge
    /// whose target is any node matching `symbol` (by qualified or plain name).
    pub fn callers(&self, symbol: &str, limit: usize) -> Result<Vec<CallEdge>> {
        self.call_edges(symbol, Direction::Callers, limit)
    }

    /// Find callees of `symbol`: nodes on the target side of a `calls` edge
    /// whose source is any node matching `symbol`.
    pub fn callees(&self, symbol: &str, limit: usize) -> Result<Vec<CallEdge>> {
        self.call_edges(symbol, Direction::Callees, limit)
    }

    fn call_edges(&self, symbol: &str, dir: Direction, limit: usize) -> Result<Vec<CallEdge>> {
        let ids = self.resolve_ids(symbol)?;
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        // `edge_side` is the endpoint pinned to `symbol`; `node_side` is the
        // endpoint hydrated into the returned node.
        let (edge_side, node_side) = match dir {
            Direction::Callers => ("e.target", "e.source"),
            Direction::Callees => ("e.source", "e.target"),
        };
        let placeholders = placeholders(ids.len());
        let sql = format!(
            "SELECT {NODE_COLUMNS}, e.line \
             FROM edges e JOIN nodes n ON n.id = {node_side} \
             WHERE e.kind = 'calls' AND {edge_side} IN ({placeholders}) \
             ORDER BY n.file_path ASC, n.start_line ASC \
             LIMIT {}",
            limit.max(1)
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(ids.iter()), |row| {
                Ok(CallEdge {
                    node: row_to_node(row)?,
                    line: row.get::<_, Option<i64>>(11)?.map(|l| l as u32),
                })
            })?
            .filter_map(std::result::Result::ok)
            .collect();
        Ok(rows)
    }

    /// Downstream impact: nodes that (transitively) depend on `symbol`, walking
    /// `calls`/`references`/`imports` edges backwards from the symbol's node(s),
    /// bounded by `depth`. The seed nodes themselves are excluded. Cycle-safe:
    /// the recursive `UNION` dedups and the depth bound guarantees termination.
    pub fn impact(&self, symbol: &str, depth: usize, limit: usize) -> Result<Vec<GraphNode>> {
        // Bail early if the symbol resolves to nothing, so an empty result is
        // unambiguously "no dependents" rather than "unknown symbol".
        if self.resolve_ids(symbol)?.is_empty() {
            return Ok(Vec::new());
        }
        let sql = format!(
            "WITH RECURSIVE impact(id, depth) AS ( \
                 SELECT id, 0 FROM nodes WHERE qualified_name = :sym OR name = :sym \
                 UNION \
                 SELECT e.source, impact.depth + 1 \
                 FROM edges e JOIN impact ON e.target = impact.id \
                 WHERE e.kind IN ('calls','references','imports') AND impact.depth < :depth \
             ) \
             SELECT DISTINCT {NODE_COLUMNS} \
             FROM impact JOIN nodes n ON n.id = impact.id \
             WHERE n.qualified_name != :sym AND n.name != :sym \
             ORDER BY n.file_path ASC, n.start_line ASC \
             LIMIT {}",
            limit.max(1)
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(
                rusqlite::named_params! {
                    ":sym": symbol,
                    ":depth": depth as i64,
                },
                row_to_node,
            )?
            .filter_map(std::result::Result::ok)
            .collect();
        Ok(rows)
    }

    /// Summary counts for the database.
    ///
    /// Counts come from `files`/`nodes`/`edges`. `project_metadata` is
    /// deliberately not consulted: CodeGraph 0.9.7 never writes it (see the
    /// crate docs), so it is always empty and cannot be a source of truth.
    pub fn status(&self) -> Result<GraphStatus> {
        let (file_count, node_count, edge_count) = self.conn.query_row(
            "SELECT (SELECT COUNT(*) FROM files), \
                    (SELECT COUNT(*) FROM nodes), \
                    (SELECT COUNT(*) FROM edges)",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)? as u64,
                    row.get::<_, i64>(1)? as u64,
                    row.get::<_, i64>(2)? as u64,
                ))
            },
        )?;
        Ok(GraphStatus {
            schema_version: self.schema_version,
            file_count,
            node_count,
            edge_count,
        })
    }

    /// Distinct, non-empty `file_path`s recorded in `nodes`, sorted. This backs
    /// the `list_files` surface; filtering/formatting is the caller's concern.
    pub fn file_paths(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT file_path FROM nodes \
             WHERE file_path IS NOT NULL AND file_path != '' ORDER BY file_path",
        )?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .filter_map(std::result::Result::ok)
            .collect();
        Ok(rows)
    }

    /// Build a relevance-ordered set of nodes for a natural-language `task`.
    ///
    /// Strategy (issue #78, Phase 1): FTS seed on the task's significant terms,
    /// then a bounded breadth-first expansion (≤2 hops) over
    /// `calls`/`references`/`contains`/`imports` edges, ordered by hop distance
    /// then symbol-kind weight. This does not replicate CodeGraph's ranking —
    /// it produces a *useful* candidate set that the caller's reranker refines
    /// on top of (MCP `get_context` re-ranks the result and only consumes the
    /// node list).
    pub fn context(&self, task: &str, max_nodes: usize) -> Result<Vec<GraphNode>> {
        let cap = max_nodes.max(1);
        let seeds = self.context_seeds(task, cap)?;
        if seeds.is_empty() {
            return Ok(Vec::new());
        }

        // hop distance per node id; seeds are hop 0.
        let mut hop: HashMap<String, u32> = HashMap::new();
        for n in &seeds {
            hop.entry(n.id.clone()).or_insert(0);
        }

        const MAX_HOP: u32 = 2;
        let mut frontier: Vec<String> = seeds.iter().map(|n| n.id.clone()).collect();
        let mut h = 1;
        while h <= MAX_HOP && hop.len() < cap {
            let mut next = Vec::new();
            for id in self.neighbor_ids(&frontier)? {
                // Stop the moment the budget is reached: a hyper-central seed can
                // have far more incident edges than `cap`, and admitting them all
                // would blow the bound (and, via the `IN (…)` hydration, the
                // SQLite bind-variable limit). The bound must match the docstring.
                if hop.len() >= cap {
                    break;
                }
                if !hop.contains_key(&id) {
                    hop.insert(id.clone(), h);
                    next.push(id);
                }
            }
            if next.is_empty() || hop.len() >= cap {
                break;
            }
            frontier = next;
            h += 1;
        }

        let ids: Vec<String> = hop.keys().cloned().collect();
        let mut nodes = self.nodes_by_ids(&ids)?;
        nodes.sort_by(|a, b| {
            let ha = hop.get(&a.id).copied().unwrap_or(u32::MAX);
            let hb = hop.get(&b.id).copied().unwrap_or(u32::MAX);
            ha.cmp(&hb)
                .then_with(|| kind_weight(&b.kind).cmp(&kind_weight(&a.kind)))
                .then_with(|| a.qualified_name.cmp(&b.qualified_name))
        });
        nodes.truncate(cap);
        Ok(nodes)
    }

    /// FTS seed nodes for [`GraphDb::context`]: match the task's significant
    /// terms (OR'd), falling back to a whole-string query when the task has no
    /// usable terms, and to per-term `LIKE` when FTS yields nothing.
    fn context_seeds(&self, task: &str, limit: usize) -> Result<Vec<GraphNode>> {
        let tokens = significant_tokens(task);
        if tokens.is_empty() {
            return self.query(task, None, limit);
        }

        let match_expr = tokens
            .iter()
            .map(|t| fts_match_phrase(t))
            .collect::<Vec<_>>()
            .join(" OR ");
        let sql = format!(
            "SELECT {NODE_COLUMNS} \
             FROM nodes_fts JOIN nodes n ON n.rowid = nodes_fts.rowid \
             WHERE nodes_fts MATCH ?1 ORDER BY nodes_fts.rank LIMIT {}",
            limit.max(1)
        );
        let params: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(match_expr)];
        let fts = self.collect_nodes(&sql, params).unwrap_or_default();
        if !fts.is_empty() {
            return Ok(fts);
        }

        // LIKE fallback: any token as a substring of name/qualified_name.
        let mut clauses = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        for (i, tok) in tokens.iter().enumerate() {
            let p = i + 1;
            clauses.push(format!(
                "n.name LIKE ?{p} ESCAPE '\\' OR n.qualified_name LIKE ?{p} ESCAPE '\\'"
            ));
            params.push(Box::new(format!("%{}%", escape_like(tok))));
        }
        let sql = format!(
            "SELECT {NODE_COLUMNS} FROM nodes n WHERE {} \
             ORDER BY (n.end_line - n.start_line) ASC LIMIT {}",
            clauses.join(" OR "),
            limit.max(1)
        );
        self.collect_nodes(&sql, params)
    }

    /// Distinct node ids adjacent to any node in `frontier` via a
    /// context-relevant edge kind (in either direction).
    fn neighbor_ids(&self, frontier: &[String]) -> Result<Vec<String>> {
        if frontier.is_empty() {
            return Ok(Vec::new());
        }
        let ph = placeholders(frontier.len());
        let sql = format!(
            "SELECT source, target FROM edges \
             WHERE kind IN ('calls','references','contains','imports') \
               AND (source IN ({ph}) OR target IN ({ph}))"
        );
        // `frontier` is bound twice (once per IN clause).
        let bound: Vec<&String> = frontier.iter().chain(frontier.iter()).collect();
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(bound), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .filter_map(std::result::Result::ok);
        let mut out = Vec::new();
        for (src, tgt) in rows {
            out.push(src);
            out.push(tgt);
        }
        out.sort();
        out.dedup();
        Ok(out)
    }

    /// Hydrate a set of node ids into [`GraphNode`]s (order unspecified).
    ///
    /// Chunks the `IN (…)` so an arbitrarily large id set can never exceed
    /// `SQLITE_MAX_VARIABLE_NUMBER` — belt-and-suspenders alongside the caller's
    /// budget cap.
    fn nodes_by_ids(&self, ids: &[String]) -> Result<Vec<GraphNode>> {
        // Well under SQLITE_MAX_VARIABLE_NUMBER on both modern (32766) and
        // legacy (999) SQLite builds.
        const CHUNK: usize = 900;
        let mut out = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(CHUNK) {
            let ph = placeholders(chunk.len());
            let sql = format!("SELECT {NODE_COLUMNS} FROM nodes n WHERE n.id IN ({ph})");
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt
                .query_map(rusqlite::params_from_iter(chunk.iter()), row_to_node)?
                .filter_map(std::result::Result::ok);
            out.extend(rows);
        }
        Ok(out)
    }

    /// Resolve `symbol` to node id(s), preferring an exact `qualified_name`
    /// match over a plain `name` match. Multiple ids are returned when the name
    /// is ambiguous (e.g. same method name across languages/files).
    fn resolve_ids(&self, symbol: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT id FROM nodes WHERE qualified_name = ?1 OR name = ?1 \
             ORDER BY CASE WHEN qualified_name = ?1 THEN 0 ELSE 1 END",
        )?;
        let ids = stmt
            .query_map([symbol], |row| row.get::<_, String>(0))?
            .filter_map(std::result::Result::ok)
            .collect();
        Ok(ids)
    }

    fn collect_nodes(
        &self,
        sql: &str,
        params: Vec<Box<dyn rusqlite::ToSql>>,
    ) -> Result<Vec<GraphNode>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params.iter()), row_to_node)?
            .filter_map(std::result::Result::ok)
            .collect();
        Ok(rows)
    }
}

enum Direction {
    Callers,
    Callees,
}

/// Read the highest applied schema version. Errors if `schema_versions` is
/// missing/unreadable — that indicates a corrupt or non-CodeGraph database.
fn read_schema_version(conn: &Connection, db_path: &Path) -> Result<i64> {
    let path = db_path.display().to_string();
    conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_versions",
        [],
        |row| row.get::<_, i64>(0),
    )
    .optional()
    .map_err(|source| Error::Invalid {
        path: path.clone(),
        detail: format!("cannot read schema_versions ({source})"),
    })?
    .ok_or_else(|| Error::Invalid {
        path,
        detail: "schema_versions has no rows; cannot verify compatibility".to_string(),
    })
}

/// Map a row selecting [`NODE_COLUMNS`] (optionally with trailing extra
/// columns) into a [`GraphNode`].
fn row_to_node(row: &Row<'_>) -> rusqlite::Result<GraphNode> {
    Ok(GraphNode {
        id: row.get(0)?,
        kind: row.get(1)?,
        name: row.get(2)?,
        qualified_name: row.get(3)?,
        file_path: row.get(4)?,
        language: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
        start_line: row.get::<_, Option<i64>>(6)?.unwrap_or(0) as u32,
        end_line: row.get::<_, Option<i64>>(7)?.unwrap_or(0) as u32,
        signature: row.get(8)?,
        docstring: row.get(9)?,
        is_async: row.get::<_, Option<i64>>(10)?.unwrap_or(0) != 0,
    })
}

/// Wrap arbitrary user input as a single FTS5 phrase literal, doubling internal
/// quotes. This prevents FTS5 from interpreting `-`, `*`, `:`, `(`, `"` etc. as
/// query operators and rejecting the input.
fn fts_match_phrase(term: &str) -> String {
    format!("\"{}\"", term.replace('"', "\"\""))
}

/// Escape SQL `LIKE` metacharacters so `search` is matched literally (paired
/// with `ESCAPE '\'` in the query).
fn escape_like(search: &str) -> String {
    let mut out = String::with_capacity(search.len());
    for ch in search.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Build `?,?,…` with `n` placeholders for an `IN (…)` clause.
fn placeholders(n: usize) -> String {
    vec!["?"; n].join(",")
}

/// Render a filesystem path as a SQLite `file:` URI so the connection can pass
/// `?immutable=1`. Backslashes become forward slashes; Windows drive paths
/// (`C:/…`) get the `file:///` authority form; `%`/`?`/`#` are percent-encoded
/// so they aren't mistaken for URI query/fragment syntax.
fn to_sqlite_uri(path: &Path) -> String {
    let forward = path.to_string_lossy().replace('\\', "/");
    let mut encoded = String::with_capacity(forward.len() + 8);
    for ch in forward.chars() {
        match ch {
            '%' => encoded.push_str("%25"),
            '?' => encoded.push_str("%3f"),
            '#' => encoded.push_str("%23"),
            _ => encoded.push(ch),
        }
    }
    // Unix absolute paths already start with '/', giving file:///abs; Windows
    // drive paths (C:/…) need the extra leading slash to reach file:///C:/….
    if encoded.starts_with('/') {
        format!("file://{encoded}?immutable=1")
    } else {
        format!("file:///{encoded}?immutable=1")
    }
}

/// Relevance weight of a node kind for context ordering: definitions that carry
/// behaviour rank above containers, which rank above imports/variables.
fn kind_weight(kind: &str) -> u8 {
    match kind {
        "function" | "method" => 3,
        "class" | "struct" | "enum" => 2,
        _ => 1,
    }
}

/// Split a natural-language task into significant search terms: alphanumeric/
/// underscore runs of length ≥3, lowercased, de-duplicated (order-preserving),
/// minus a few common English stopwords, capped to keep the FTS query small.
fn significant_tokens(task: &str) -> Vec<String> {
    const STOPWORDS: &[&str] = &[
        "the", "and", "for", "with", "that", "this", "from", "into", "how", "does", "what", "when",
        "where", "which", "all", "any", "get", "set", "use", "using", "via",
    ];
    const MAX_TOKENS: usize = 10;

    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for raw in task.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        if raw.len() < 3 {
            continue;
        }
        let tok = raw.to_lowercase();
        if STOPWORDS.contains(&tok.as_str()) || !seen.insert(tok.clone()) {
            continue;
        }
        out.push(tok);
        if out.len() >= MAX_TOKENS {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Path to the committed golden fixture. The reader's compatibility
    /// contract — a required artifact, not an optional one: these tests fail
    /// (rather than skip) if it is missing.
    fn fixture_path() -> PathBuf {
        // CARGO_MANIFEST_DIR = <repo>/src/semgraph; fixture is at <repo>/tests/…
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/codegraph-v4.db")
    }

    fn open() -> GraphDb {
        let path = fixture_path();
        assert!(
            path.exists(),
            "required fixture missing at {} — build it per tests/fixtures/README.md",
            path.display()
        );
        GraphDb::open(&path).expect("fixture opens and passes the schema guard")
    }

    #[test]
    fn opens_and_reports_schema_v4() {
        let db = open();
        assert_eq!(db.schema_version(), 4);
    }

    #[test]
    fn status_matches_fixture_counts() {
        let db = open();
        let s = db.status().unwrap();
        assert_eq!(
            s,
            GraphStatus {
                schema_version: 4,
                file_count: 7,
                node_count: 55,
                edge_count: 116,
            }
        );
    }

    #[test]
    fn empty_unresolved_refs_and_project_metadata_are_normal() {
        // CodeGraph 0.9.7 always leaves these two tables empty. The reader must
        // treat 0 rows as expected: open() and status() must succeed regardless.
        let db = open();
        let unresolved: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM unresolved_refs", [], |r| r.get(0))
            .unwrap();
        let metadata: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM project_metadata", [], |r| r.get(0))
            .unwrap();
        assert_eq!(unresolved, 0, "unresolved_refs is expected to be empty");
        assert_eq!(metadata, 0, "project_metadata is expected to be empty");
        // status() derives counts without project_metadata, so it still works.
        assert_eq!(db.status().unwrap().node_count, 55);
    }

    #[test]
    fn query_finds_symbol_via_fts() {
        let db = open();
        let hits = db.query("circle_area", None, 10).unwrap();
        assert!(
            hits.iter()
                .any(|n| n.name == "circle_area" && n.kind == "function"),
            "expected circle_area function, got {hits:?}"
        );
    }

    #[test]
    fn query_kind_filter_restricts_results() {
        let db = open();
        let hits = db.query("area", Some("method"), 10).unwrap();
        assert!(!hits.is_empty(), "expected at least one method named area");
        assert!(
            hits.iter().all(|n| n.kind == "method"),
            "kind filter leaked non-method rows: {hits:?}"
        );
    }

    #[test]
    fn query_falls_back_to_like_for_partial_tokens() {
        let db = open();
        // "ircle" is a partial token FTS won't match but LIKE '%ircle%' will.
        let hits = db.query("ircle", None, 10).unwrap();
        assert!(
            hits.iter().any(|n| n.name == "Circle"),
            "LIKE fallback should surface Circle for 'ircle', got {hits:?}"
        );
    }

    #[test]
    fn query_tolerates_fts_operator_characters() {
        let db = open();
        // A bare special char must not error — it should just yield no/other rows.
        let _ = db.query("-", None, 5).unwrap();
        let _ = db.query("Point::new", None, 5).unwrap();
    }

    #[test]
    fn callers_of_circle_area() {
        let db = open();
        let callers: Vec<String> = db
            .callers("circle_area", 50)
            .unwrap()
            .into_iter()
            .map(|c| c.node.qualified_name)
            .collect();
        // circle_area is called by summarize() and Circle::area (python).
        assert!(callers.contains(&"summarize".to_string()), "{callers:?}");
        assert!(callers.contains(&"Circle::area".to_string()), "{callers:?}");
    }

    #[test]
    fn callees_of_summarize_include_call_site_line() {
        let db = open();
        let callees = db.callees("summarize", 50).unwrap();
        let ca = callees
            .iter()
            .find(|c| c.node.name == "circle_area")
            .expect("summarize should call circle_area");
        // The fixture records this call at line 27.
        assert_eq!(ca.line, Some(27));
    }

    #[test]
    fn impact_of_circle_area_reaches_transitive_dependents() {
        let db = open();
        let names: Vec<String> = db
            .impact("circle_area", 3, 100)
            .unwrap()
            .into_iter()
            .map(|n| n.qualified_name)
            .collect();
        // Direct dependent (calls circle_area) and a transitive one (calls summarize).
        assert!(names.contains(&"summarize".to_string()), "{names:?}");
        assert!(
            names.contains(&"gather_measurements".to_string()),
            "transitive dependent missing: {names:?}"
        );
        // The seed itself must be excluded.
        assert!(!names.contains(&"circle_area".to_string()), "{names:?}");
    }

    #[test]
    fn unknown_symbol_yields_empty_not_error() {
        let db = open();
        assert!(db.callers("no_such_symbol_xyz", 10).unwrap().is_empty());
        assert!(db.callees("no_such_symbol_xyz", 10).unwrap().is_empty());
        assert!(db.impact("no_such_symbol_xyz", 2, 10).unwrap().is_empty());
    }

    #[test]
    fn empty_search_short_circuits_to_no_results() {
        // Empty/whitespace input must not fall through to `LIKE '%%'` and match
        // every node; it returns nothing.
        let db = open();
        assert!(db.query("", None, 50).unwrap().is_empty());
        assert!(db.query("   ", None, 50).unwrap().is_empty());
        // Sanity: the same DB does return rows for a real term.
        assert!(!db.query("Circle", None, 50).unwrap().is_empty());
    }

    // ---- Schema-guard / error-path coverage (temp DBs) --------------------

    use tempfile::TempDir;

    /// Create a minimal but valid schema-v4 DB with zero graph rows.
    fn make_empty_v4_db(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_versions(version INTEGER NOT NULL, applied_at INTEGER, description TEXT);
             INSERT INTO schema_versions(version) VALUES (4);
             CREATE TABLE nodes(id TEXT PRIMARY KEY, kind TEXT NOT NULL, name TEXT NOT NULL, \
                 qualified_name TEXT NOT NULL, file_path TEXT NOT NULL, language TEXT NOT NULL, \
                 start_line INTEGER NOT NULL, end_line INTEGER NOT NULL, signature TEXT, \
                 docstring TEXT, is_async INTEGER DEFAULT 0);
             CREATE TABLE edges(id INTEGER PRIMARY KEY AUTOINCREMENT, source TEXT NOT NULL, \
                 target TEXT NOT NULL, kind TEXT NOT NULL, line INTEGER);
             CREATE TABLE files(path TEXT PRIMARY KEY);
             CREATE TABLE unresolved_refs(id INTEGER PRIMARY KEY AUTOINCREMENT, from_node_id TEXT);
             CREATE TABLE project_metadata(key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE VIRTUAL TABLE nodes_fts USING fts5(id, name, qualified_name, docstring, \
                 signature, content='nodes', content_rowid='rowid');",
        )
        .unwrap();
    }

    #[test]
    fn rejects_newer_schema_version_with_actionable_error() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("v5.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE schema_versions(version INTEGER NOT NULL); \
                 INSERT INTO schema_versions(version) VALUES (4),(5);",
            )
            .unwrap();
        }
        match GraphDb::open(&path) {
            Err(Error::UnsupportedSchema {
                found, supported, ..
            }) => {
                assert_eq!(found, 5);
                assert_eq!(supported, SUPPORTED_SCHEMA_VERSION);
            }
            Err(e) => panic!("expected UnsupportedSchema, got {e:?}"),
            Ok(_) => panic!("expected UnsupportedSchema, got Ok"),
        }
        let msg = GraphDb::open(&path).unwrap_err().to_string();
        assert!(msg.contains("schema version 5"), "not actionable: {msg}");
    }

    #[test]
    fn missing_schema_versions_table_is_invalid() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("noschema.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("CREATE TABLE nodes(id TEXT);").unwrap();
        }
        match GraphDb::open(&path) {
            Err(Error::Invalid { .. }) => {}
            Err(e) => panic!("expected Invalid, got {e:?}"),
            Ok(_) => panic!("expected Invalid, got Ok"),
        }
    }

    #[test]
    fn missing_file_is_open_error() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("does-not-exist.db");
        match GraphDb::open(&path) {
            Err(Error::Open { .. }) => {}
            Err(e) => panic!("expected Open, got {e:?}"),
            Ok(_) => panic!("expected Open, got Ok"),
        }
    }

    #[test]
    fn empty_v4_db_reads_as_empty() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("empty.db");
        make_empty_v4_db(&path);
        let db = GraphDb::open(&path).unwrap();
        assert_eq!(db.schema_version(), 4);
        assert_eq!(
            db.status().unwrap(),
            GraphStatus {
                schema_version: 4,
                file_count: 0,
                node_count: 0,
                edge_count: 0,
            }
        );
        assert!(db.query("anything", None, 10).unwrap().is_empty());
        assert!(db.callers("x", 10).unwrap().is_empty());
        assert!(db.callees("x", 10).unwrap().is_empty());
        assert!(db.impact("x", 2, 10).unwrap().is_empty());
    }

    #[test]
    fn immutable_open_creates_no_sidecars_on_wal_db() {
        // Copy the WAL-mode fixture into a temp dir; an immutable open must read
        // it without spawning -wal/-shm next to it.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("copy.db");
        std::fs::copy(fixture_path(), &path).unwrap();
        {
            let db = GraphDb::open(&path).unwrap();
            assert_eq!(db.status().unwrap().node_count, 55);
            let _ = db.query("Circle", None, 5).unwrap();
        }
        let wal = PathBuf::from(format!("{}-wal", path.display()));
        let shm = PathBuf::from(format!("{}-shm", path.display()));
        assert!(!wal.exists(), "immutable open must not create -wal");
        assert!(!shm.exists(), "immutable open must not create -shm");
    }

    #[test]
    fn sqlite_uri_encoding_forms() {
        assert_eq!(
            to_sqlite_uri(Path::new("/home/u/x.db")),
            "file:///home/u/x.db?immutable=1"
        );
        assert_eq!(
            to_sqlite_uri(Path::new(r"C:\a\b.db")),
            "file:///C:/a/b.db?immutable=1"
        );
        assert!(to_sqlite_uri(Path::new("/tmp/a?b#c.db")).contains("%3f"));
    }

    #[test]
    fn context_seeds_and_expands_from_task_terms() {
        let db = open();
        let names: Vec<String> = db
            .context("compute the area of a circle", 25)
            .unwrap()
            .into_iter()
            .map(|n| n.qualified_name)
            .collect();
        // FTS seeds on "area"/"circle" surface circle_area; expansion over
        // calls edges pulls in its caller summarize.
        assert!(names.contains(&"circle_area".to_string()), "{names:?}");
        assert!(names.contains(&"summarize".to_string()), "{names:?}");
    }

    #[test]
    fn context_respects_max_nodes_and_empties_gracefully() {
        let db = open();
        assert!(db.context("area circle", 3).unwrap().len() <= 3);
        // A task with no matching terms yields an empty set, not an error.
        assert!(db
            .context("zzz nonexistent gibberish", 10)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn significant_tokens_filters_stopwords_and_shorts() {
        let toks = significant_tokens("How does the Circle area getter work?");
        assert!(toks.contains(&"circle".to_string()));
        assert!(toks.contains(&"area".to_string()));
        assert!(!toks.iter().any(|t| t == "the" || t == "how" || t == "does"));
    }

    #[test]
    fn context_bounds_a_hyper_central_node_without_erroring() {
        // A seed whose incident-edge count exceeds SQLITE_MAX_VARIABLE_NUMBER
        // (32766) must not blow the bind limit: context() has to cap the BFS
        // frontier at max_nodes, not admit every neighbour then truncate.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("fanout.db");
        make_empty_v4_db(&path);
        let fan = 33_000;
        {
            let mut conn = Connection::open(&path).unwrap();
            let tx = conn.transaction().unwrap();
            tx.execute(
                "INSERT INTO nodes(id,kind,name,qualified_name,file_path,language,\
                 start_line,end_line) VALUES('function:center','function','center',\
                 'center','f.rs','rust',1,2)",
                [],
            )
            .unwrap();
            {
                let mut ins_node = tx
                    .prepare(
                        "INSERT INTO nodes(id,kind,name,qualified_name,file_path,language,\
                         start_line,end_line) VALUES(?1,'function',?2,?2,'f.rs','rust',10,11)",
                    )
                    .unwrap();
                let mut ins_edge = tx
                    .prepare("INSERT INTO edges(source,target,kind) VALUES('function:center',?1,'calls')")
                    .unwrap();
                for i in 0..fan {
                    let leaf = format!("function:leaf{i}");
                    ins_node
                        .execute(rusqlite::params![leaf, format!("leaf{i}")])
                        .unwrap();
                    ins_edge.execute(rusqlite::params![leaf]).unwrap();
                }
            }
            tx.commit().unwrap();
        }

        let db = GraphDb::open(&path).unwrap();
        // Seeds on the central node (LIKE fallback — the temp DB has no FTS
        // triggers); its 33k-edge fan-out must not error and must respect the cap.
        let res = db.context("center", 10).unwrap();
        assert!(res.len() <= 10, "cap not respected: got {}", res.len());
        assert!(
            res.iter().any(|n| n.name == "center"),
            "seed must be present"
        );
    }
}

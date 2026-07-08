//! Native Rust reader for a CodeGraph `codegraph.db` (SQLite, schema v4).
//!
//! This crate owns the read path for a bundle's graph database, replacing the
//! former shell-out to the `codegraph` CLI for query operations (issue #78,
//! Phase 1). It is a standalone library so the read path and the future Phase 2
//! writer (used by `sembundle`) share one definition of the schema and types.
//! All access here is read-only and scoped to a single database file.
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

use std::path::Path;

use rusqlite::{Connection, OpenFlags, OptionalExtension, Row};
use thiserror::Error;

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
pub struct GraphDb {
    conn: Connection,
    schema_version: i64,
}

impl GraphDb {
    /// Open `db_path` read-only and validate its schema version.
    ///
    /// Returns [`Error::UnsupportedSchema`] when the database declares a schema
    /// newer than [`SUPPORTED_SCHEMA_VERSION`].
    pub fn open(db_path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(
            db_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
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
}

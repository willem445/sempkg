//! Writer for a schema-v4 CodeGraph `codegraph.db` (issue #78, Phase 2a).
//!
//! Produces a database byte-compatible with what CodeGraph 0.9.7 emits: the
//! same tables, indexes, FTS5 contentless-external table with its
//! insert/update/delete triggers, and a `schema_versions` row declaring v4. The
//! reader ([`crate::GraphDb`]) opens the result unchanged.
//!
//! Writes are batched into a **single transaction** for throughput; the FTS
//! index is maintained by the `nodes_ai`/`nodes_au`/`nodes_ad` triggers exactly
//! as in a CodeGraph-built DB (no manual `nodes_fts` upkeep here). A final
//! `ANALYZE` populates `sqlite_stat1` so the query planner sees the same stats
//! surface a CodeGraph DB ships with.

use std::path::Path;

use rusqlite::Connection;

use crate::model::{EdgeRecord, FileRecord, NodeRecord};
use crate::{Error, Result};

/// The schema-v4 DDL: tables, indexes, FTS5 table + triggers. Mirrors a
/// CodeGraph 0.9.7 database structurally (verified against the golden fixture
/// `tests/fixtures/codegraph-v4.db`).
const SCHEMA_DDL: &str = r#"
CREATE TABLE nodes (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    qualified_name TEXT NOT NULL,
    file_path TEXT NOT NULL,
    language TEXT NOT NULL,
    start_line INTEGER NOT NULL,
    end_line INTEGER NOT NULL,
    start_column INTEGER NOT NULL,
    end_column INTEGER NOT NULL,
    docstring TEXT,
    signature TEXT,
    visibility TEXT,
    is_exported INTEGER DEFAULT 0,
    is_async INTEGER DEFAULT 0,
    is_static INTEGER DEFAULT 0,
    is_abstract INTEGER DEFAULT 0,
    decorators TEXT,
    type_parameters TEXT,
    updated_at INTEGER NOT NULL
);
CREATE TABLE edges (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    source TEXT NOT NULL,
    target TEXT NOT NULL,
    kind TEXT NOT NULL,
    metadata TEXT,
    line INTEGER,
    col INTEGER,
    provenance TEXT DEFAULT NULL,
    FOREIGN KEY (source) REFERENCES nodes(id) ON DELETE CASCADE,
    FOREIGN KEY (target) REFERENCES nodes(id) ON DELETE CASCADE
);
CREATE TABLE files (
    path TEXT PRIMARY KEY,
    content_hash TEXT NOT NULL,
    language TEXT NOT NULL,
    size INTEGER NOT NULL,
    modified_at INTEGER NOT NULL,
    indexed_at INTEGER NOT NULL,
    node_count INTEGER DEFAULT 0,
    errors TEXT
);
CREATE TABLE unresolved_refs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    from_node_id TEXT NOT NULL,
    reference_name TEXT NOT NULL,
    reference_kind TEXT NOT NULL,
    line INTEGER NOT NULL,
    col INTEGER NOT NULL,
    candidates TEXT,
    file_path TEXT NOT NULL DEFAULT '',
    language TEXT NOT NULL DEFAULT 'unknown',
    FOREIGN KEY (from_node_id) REFERENCES nodes(id) ON DELETE CASCADE
);
CREATE TABLE project_metadata (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE schema_versions (
    version INTEGER PRIMARY KEY,
    applied_at INTEGER NOT NULL,
    description TEXT
);
CREATE VIRTUAL TABLE nodes_fts USING fts5(
    id,
    name,
    qualified_name,
    docstring,
    signature,
    content='nodes',
    content_rowid='rowid'
);
CREATE TRIGGER nodes_ai AFTER INSERT ON nodes BEGIN
    INSERT INTO nodes_fts(rowid, id, name, qualified_name, docstring, signature)
    VALUES (NEW.rowid, NEW.id, NEW.name, NEW.qualified_name, NEW.docstring, NEW.signature);
END;
CREATE TRIGGER nodes_ad AFTER DELETE ON nodes BEGIN
    INSERT INTO nodes_fts(nodes_fts, rowid, id, name, qualified_name, docstring, signature)
    VALUES ('delete', OLD.rowid, OLD.id, OLD.name, OLD.qualified_name, OLD.docstring, OLD.signature);
END;
CREATE TRIGGER nodes_au AFTER UPDATE ON nodes BEGIN
    INSERT INTO nodes_fts(nodes_fts, rowid, id, name, qualified_name, docstring, signature)
    VALUES ('delete', OLD.rowid, OLD.id, OLD.name, OLD.qualified_name, OLD.docstring, OLD.signature);
    INSERT INTO nodes_fts(rowid, id, name, qualified_name, docstring, signature)
    VALUES (NEW.rowid, NEW.id, NEW.name, NEW.qualified_name, NEW.docstring, NEW.signature);
END;
CREATE INDEX idx_nodes_kind ON nodes(kind);
CREATE INDEX idx_nodes_name ON nodes(name);
CREATE INDEX idx_nodes_lower_name ON nodes(lower(name));
CREATE INDEX idx_nodes_qualified_name ON nodes(qualified_name);
CREATE INDEX idx_nodes_file_path ON nodes(file_path);
CREATE INDEX idx_nodes_file_line ON nodes(file_path, start_line);
CREATE INDEX idx_nodes_language ON nodes(language);
CREATE INDEX idx_edges_kind ON edges(kind);
CREATE INDEX idx_edges_source_kind ON edges(source, kind);
CREATE INDEX idx_edges_target_kind ON edges(target, kind);
CREATE INDEX idx_edges_provenance ON edges(provenance);
CREATE INDEX idx_files_language ON files(language);
CREATE INDEX idx_files_modified_at ON files(modified_at);
CREATE INDEX idx_unresolved_from_node ON unresolved_refs(from_node_id);
CREATE INDEX idx_unresolved_name ON unresolved_refs(reference_name);
CREATE INDEX idx_unresolved_from_name ON unresolved_refs(from_node_id, reference_name);
CREATE INDEX idx_unresolved_file_path ON unresolved_refs(file_path);
"#;

/// A single-writer handle that creates a schema-v4 database and fills it.
///
/// Typical use: [`GraphWriter::create`], one [`GraphWriter::write`] with all
/// records, then [`GraphWriter::finalize`]. Records are inserted inside one
/// transaction so a partial index never lands on disk.
pub struct GraphWriter {
    conn: Connection,
}

impl GraphWriter {
    /// Create a fresh database at `db_path`, installing the full schema-v4
    /// structure. An existing file at `db_path` is truncated/overwritten.
    pub fn create(db_path: &Path) -> Result<GraphWriter> {
        // Start from an empty file so a stale DB can't leak rows into the new
        // index. `Connection::open` creates or opens; we drop any existing one.
        if db_path.exists() {
            std::fs::remove_file(db_path).map_err(|e| Error::Invalid {
                path: db_path.display().to_string(),
                detail: format!("cannot overwrite existing database: {e}"),
            })?;
        }
        let conn = Connection::open(db_path).map_err(|source| Error::Open {
            path: db_path.display().to_string(),
            source,
        })?;
        // FK cascade (edges → nodes) is part of the schema contract; enable it
        // so Phase 2b incremental deletes cascade like CodeGraph's do.
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.execute_batch(SCHEMA_DDL)?;
        write_schema_versions(&conn)?;
        Ok(GraphWriter { conn })
    }

    /// Wrap an already-open connection to a schema-v4 database for incremental
    /// writes (Phase 2b `sync`). Unlike [`GraphWriter::create`] this does **not**
    /// install the schema — the database already exists. The caller is
    /// responsible for enabling `foreign_keys` if cascade deletes are needed.
    pub(crate) fn from_connection(conn: Connection) -> GraphWriter {
        GraphWriter { conn }
    }

    /// Read every node back as a [`NodeRecord`], preserving stored ids. Used by
    /// `sync` to rebuild the resolver's symbol table over the current graph
    /// after incremental node inserts/deletes.
    pub(crate) fn all_nodes(&self) -> Result<Vec<NodeRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, name, qualified_name, file_path, language, \
                    start_line, end_line, start_column, end_column, \
                    docstring, signature, visibility, \
                    is_exported, is_async, is_static, is_abstract, \
                    decorators, type_parameters, updated_at FROM nodes",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(NodeRecord {
                    id: r.get(0)?,
                    kind: r.get(1)?,
                    name: r.get(2)?,
                    qualified_name: r.get(3)?,
                    file_path: r.get(4)?,
                    language: r.get(5)?,
                    start_line: r.get::<_, i64>(6)? as u32,
                    end_line: r.get::<_, i64>(7)? as u32,
                    start_column: r.get::<_, i64>(8)? as u32,
                    end_column: r.get::<_, i64>(9)? as u32,
                    docstring: r.get(10)?,
                    signature: r.get(11)?,
                    visibility: r.get(12)?,
                    is_exported: r.get::<_, i64>(13)? != 0,
                    is_async: r.get::<_, i64>(14)? != 0,
                    is_static: r.get::<_, i64>(15)? != 0,
                    is_abstract: r.get::<_, i64>(16)? != 0,
                    decorators: r.get(17)?,
                    type_parameters: r.get(18)?,
                    updated_at: r.get(19)?,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    /// Insert all `nodes`, `edges`, and `files` in a single transaction. The
    /// FTS index is populated by the `nodes_ai` trigger as nodes are inserted.
    pub fn write(
        &mut self,
        nodes: &[NodeRecord],
        edges: &[EdgeRecord],
        files: &[FileRecord],
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut ins_node = tx.prepare(
                "INSERT OR IGNORE INTO nodes (\
                     id, kind, name, qualified_name, file_path, language, \
                     start_line, end_line, start_column, end_column, \
                     docstring, signature, visibility, \
                     is_exported, is_async, is_static, is_abstract, \
                     decorators, type_parameters, updated_at) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)",
            )?;
            for n in nodes {
                ins_node.execute(rusqlite::params![
                    n.id,
                    n.kind,
                    n.name,
                    n.qualified_name,
                    n.file_path,
                    n.language,
                    n.start_line,
                    n.end_line,
                    n.start_column,
                    n.end_column,
                    n.docstring,
                    n.signature,
                    n.visibility,
                    n.is_exported as i64,
                    n.is_async as i64,
                    n.is_static as i64,
                    n.is_abstract as i64,
                    n.decorators,
                    n.type_parameters,
                    n.updated_at,
                ])?;
            }

            let mut ins_file = tx.prepare(
                "INSERT OR REPLACE INTO files (\
                     path, content_hash, language, size, modified_at, indexed_at, node_count, errors) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            )?;
            for f in files {
                ins_file.execute(rusqlite::params![
                    f.path,
                    f.content_hash,
                    f.language,
                    f.size as i64,
                    f.modified_at,
                    f.indexed_at,
                    f.node_count as i64,
                    f.errors,
                ])?;
            }

            let mut ins_edge = tx.prepare(
                "INSERT INTO edges (source, target, kind, metadata, line, col, provenance) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7)",
            )?;
            for e in edges {
                ins_edge.execute(rusqlite::params![
                    e.source,
                    e.target,
                    e.kind,
                    e.metadata,
                    e.line,
                    e.col,
                    e.provenance,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Run `ANALYZE` (populating `sqlite_stat1`) and close the connection.
    pub fn finalize(self) -> Result<()> {
        self.conn.execute_batch("ANALYZE;")?;
        self.conn.close().map_err(|(_, e)| Error::Sqlite(e))?;
        Ok(())
    }
}

/// Insert the `schema_versions` rows a CodeGraph DB carries: the initial v1 and
/// the current v4. The reader reads `MAX(version)` = 4.
fn write_schema_versions(conn: &Connection) -> Result<()> {
    let now = crate::index::now_millis();
    conn.execute(
        "INSERT INTO schema_versions (version, applied_at, description) VALUES (1, ?1, ?2)",
        rusqlite::params![now, "Initial schema"],
    )?;
    conn.execute(
        "INSERT INTO schema_versions (version, applied_at, description) VALUES (4, ?1, ?2)",
        rusqlite::params![now, "Initial schema includes all migrations"],
    )?;
    Ok(())
}

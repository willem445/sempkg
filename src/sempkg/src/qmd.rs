/// QMD documentation search — scoped to a specific bundle's embedded index.
///
/// Queries the QMD SQLite database (`qmd/index/index.sqlite`) inside an
/// extracted bundle directory. All searches are strictly scoped to the bundle.
///
/// Also exposes a QMD CLI wrapper for full QMD functionality on local projects.
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::Deserialize;

use crate::error::SempkgError;

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

pub fn qmd_db_path(bundle_dir: &Path) -> PathBuf {
    bundle_dir.join("qmd").join("index").join("index.sqlite")
}

pub fn qmd_metadata_path(bundle_dir: &Path) -> PathBuf {
    bundle_dir.join("qmd").join("metadata.json")
}

pub fn has_qmd(bundle_dir: &Path) -> bool {
    qmd_db_path(bundle_dir).exists()
}

// ---------------------------------------------------------------------------
// QMD metadata
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct QmdMetadata {
    pub qmd_version: Option<String>,
    pub embed_model: Option<String>,
    pub chunk_strategy: Option<String>,
    pub collection_name: Option<String>,
    pub document_count: Option<u64>,
    pub chunk_count: Option<u64>,
    pub created_at: Option<String>,
}

pub fn load_metadata(bundle_dir: &Path) -> Option<QmdMetadata> {
    let path = qmd_metadata_path(bundle_dir);
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

// ---------------------------------------------------------------------------
// SQLite search (scoped to one bundle)
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct SearchResult {
    pub path: String,
    pub snippet: String,
}

/// Full-text search against the bundle's QMD SQLite database.
///
/// `collection_name` — when provided, results are filtered to documents
/// belonging to that collection only.  This is the primary isolation
/// mechanism: even if the SQLite file was built from a global QMD run
/// that indexed multiple packages, only the named collection is searched.
///
/// Strategy (tried in order, stops at first success):
///   1. FTS5 + collection JOIN (fully scoped)
///   2. FTS5 unscoped            (fallback if collections table absent)
///   3. LIKE scan + collection JOIN
///   4. LIKE scan unscoped
pub fn search(
    bundle_dir: &Path,
    query: &str,
    limit: usize,
    collection_name: Option<&str>,
) -> Result<Vec<SearchResult>> {
    let db_path = qmd_db_path(bundle_dir);
    if !db_path.exists() {
        return Err(SempkgError::NoQmdIndex(bundle_dir.to_string_lossy().to_string()).into());
    }

    let conn = Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .context("Failed to open QMD database")?;

    // -----------------------------------------------------------------------
    // 1. FTS5 + collection filter (preferred — scoped + ranked)
    // -----------------------------------------------------------------------
    if let Some(cname) = collection_name {
        let scoped_fts = conn.prepare(
            "SELECT d.path, snippet(documents_fts, -1, '**', '**', '...', 64) AS snip \
             FROM documents AS d \
             JOIN documents_fts ON d.rowid = documents_fts.rowid \
             JOIN collections AS c ON d.collection_id = c.id \
             WHERE documents_fts MATCH ?1 AND c.name = ?2 \
             ORDER BY rank LIMIT ?3",
        )
        .and_then(|mut stmt| {
            stmt.query_map(
                rusqlite::params![query, cname, limit as i64],
                |row| Ok(SearchResult { path: row.get(0)?, snippet: row.get(1)? }),
            )
            .and_then(|rows| rows.collect::<rusqlite::Result<Vec<_>>>())
        });

        if let Ok(rows) = scoped_fts {
            return Ok(rows);
        }
    }

    // -----------------------------------------------------------------------
    // 2. FTS5 unscoped (no collections table or no collection_name provided)
    // -----------------------------------------------------------------------
    let fts_result = conn.prepare(
        "SELECT d.path, snippet(documents_fts, -1, '**', '**', '...', 64) AS snip \
         FROM documents AS d \
         JOIN documents_fts ON d.rowid = documents_fts.rowid \
         WHERE documents_fts MATCH ?1 \
         ORDER BY rank LIMIT ?2",
    )
    .and_then(|mut stmt| {
        stmt.query_map(
            rusqlite::params![query, limit as i64],
            |row| Ok(SearchResult { path: row.get(0)?, snippet: row.get(1)? }),
        )
        .and_then(|rows| rows.collect::<rusqlite::Result<Vec<_>>>())
    });

    if let Ok(rows) = fts_result {
        return Ok(rows);
    }

    // -----------------------------------------------------------------------
    // 3. LIKE scan + collection filter
    // -----------------------------------------------------------------------
    if let Some(cname) = collection_name {
        let scoped_like = conn.prepare(
            "SELECT d.path, substr(d.content, 1, 400) AS snip \
             FROM documents d \
             JOIN collections c ON d.collection_id = c.id \
             WHERE d.content LIKE ?1 AND c.name = ?2 LIMIT ?3",
        )
        .and_then(|mut stmt| {
            stmt.query_map(
                rusqlite::params![format!("%{query}%"), cname, limit as i64],
                |row| Ok(SearchResult { path: row.get(0)?, snippet: row.get(1)? }),
            )
            .and_then(|rows| rows.collect::<rusqlite::Result<Vec<_>>>())
        });

        if let Ok(rows) = scoped_like {
            return Ok(rows);
        }
    }

    // -----------------------------------------------------------------------
    // 4. LIKE scan unscoped (last resort)
    // -----------------------------------------------------------------------
    let rows = conn
        .prepare(
            "SELECT path, substr(content, 1, 400) AS snip \
             FROM documents WHERE content LIKE ?1 LIMIT ?2",
        )
        .and_then(|mut stmt| {
            stmt.query_map(
                rusqlite::params![format!("%{query}%"), limit as i64],
                |row| Ok(SearchResult { path: row.get(0)?, snippet: row.get(1)? }),
            )
            .and_then(|rows| rows.collect::<rusqlite::Result<Vec<_>>>())
        })
        .context("QMD fallback search failed")?;

    Ok(rows)
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
// QMD CLI wrapper (for local project indexing only — not used for bundle queries)
//
// Bundle doc searches always use the direct SQLite path above (search/format_results),
// which never invokes the QMD process and is therefore fully isolated by design.
//
// These functions are for when a user wants to re-index a local project with QMD.
// They run QMD with an isolated home directory so QMD cannot read or write the
// user's global collection registry (~/.config/qmd, %APPDATA%\qmd, etc.).
//
// Isolation strategy:
//   QMD's --index flag scopes the *database file* only, not the collection config.
//   To prevent QMD from touching global collections we override the OS-level
//   home/config directories so QMD's entire data footprint stays inside
//   `<project>/.sempkg/qmd-home/`.
// ---------------------------------------------------------------------------

fn qmd_exe() -> Result<String> {
    which::which("qmd")
        .or_else(|_| which::which("qmd.cmd"))
        .map(|p| p.to_string_lossy().to_string())
        .map_err(|_| SempkgError::QmdNotFound.into())
}

/// Run QMD with its home/config directories redirected to `isolated_home`.
///
/// This prevents QMD from reading or modifying the user's global collection
/// registry regardless of which OS-level path convention QMD uses.
fn run_qmd_isolated(
    args: &[&str],
    cwd: Option<&Path>,
    isolated_home: &Path,
) -> Result<String> {
    let exe = qmd_exe()?;
    std::fs::create_dir_all(isolated_home)
        .with_context(|| format!("Cannot create QMD isolated home: {}", isolated_home.display()))?;

    let mut cmd = Command::new(&exe);
    cmd.args(args);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }

    // Redirect every conventional home/config location QMD might use so that
    // it cannot read or write the user's global QMD install.
    cmd.env("QMD_HOME",        isolated_home); // hypothetical QMD-specific var
    cmd.env("QMD_DATA_DIR",    isolated_home); // hypothetical QMD-specific var
    cmd.env("XDG_DATA_HOME",   isolated_home); // Linux XDG
    cmd.env("XDG_CONFIG_HOME", isolated_home); // Linux XDG
    cmd.env("APPDATA",         isolated_home); // Windows roaming
    cmd.env("LOCALAPPDATA",    isolated_home); // Windows local

    let out = cmd.output().context("Failed to run qmd")?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if !out.status.success() {
        anyhow::bail!("{}", if !stderr.is_empty() { stderr } else { stdout });
    }
    Ok(if !stdout.is_empty() { stdout } else { stderr })
}

/// Update (re-index) a local project's scoped QMD index.
///
/// The database is stored at `<project_dir>/.sempkg/qmd/index.sqlite` and QMD's
/// entire config/data footprint is confined to `<project_dir>/.sempkg/qmd-home/`.
/// The user's global QMD collections are never touched.
///
/// `collection_name` should match the project/package name.
/// `glob_pattern` controls which files are indexed (e.g. `"**/*.{md,rst,txt}"`).
pub fn cli_update(
    project_dir: &Path,
    collection_name: &str,
    glob_pattern: &str,
) -> Result<PathBuf> {
    let sempkg_qmd = project_dir.join(".sempkg").join("qmd");
    let db_path    = sempkg_qmd.join("index").join("index.sqlite");
    let isolated   = project_dir.join(".sempkg").join("qmd-home");

    std::fs::create_dir_all(db_path.parent().unwrap())
        .context("Cannot create QMD index directory")?;

    let db = db_path.to_string_lossy().to_string();
    let proj = project_dir.to_string_lossy().to_string();

    // Step 1: register the collection into the scoped database only
    run_qmd_isolated(
        &["--index", &db, "collection", "add", &proj, "--name", collection_name, "--pattern", glob_pattern],
        Some(project_dir),
        &isolated,
    ).or_else(|e| {
        // "collection already exists" is not fatal
        if e.to_string().to_lowercase().contains("already") {
            Ok(String::new())
        } else {
            Err(e)
        }
    })?;

    // Step 2: update (crawl + embed)
    run_qmd_isolated(
        &["--index", &db, "update"],
        Some(project_dir),
        &isolated,
    )?;

    Ok(db_path)
}

/// Codegraph CLI wrapper — scoped to a specific package/bundle directory.
///
/// All queries are strictly scoped: passing a package directory means the
/// operation runs only against that package's index, never cross-package.
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use rusqlite::OptionalExtension;

use crate::error::SempkgError;

/// Resolved codegraph executable path.
fn codegraph_exe() -> Result<String> {
    which::which("codegraph")
        .or_else(|_| which::which("codegraph.cmd"))
        .map(|p| p.to_string_lossy().to_string())
        .map_err(|_| SempkgError::CodegraphNotFound.into())
}

fn run(args: &[&str], cwd: Option<&Path>) -> Result<String> {
    let exe = codegraph_exe()?;
    let mut cmd = Command::new(&exe);
    cmd.args(args);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }

    let out = cmd
        .output()
        .with_context(|| format!("Failed to run codegraph with args: {args:?}"))?;

    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();

    if !out.status.success() {
        return Err(
            SempkgError::CodegraphError(if !stderr.is_empty() { stderr } else { stdout }).into(),
        );
    }

    Ok(if !stdout.is_empty() { stdout } else { stderr })
}

// ---------------------------------------------------------------------------
// Index management
// ---------------------------------------------------------------------------

/// Run `codegraph init --index <path>` to initialise and index a project.
pub fn init_and_index(path: &Path) -> Result<String> {
    run(&["init", "--index", &path.to_string_lossy()], None).context("codegraph init failed")
}

/// Run `codegraph sync <path>` to incrementally update an existing index.
pub fn sync(path: &Path) -> Result<String> {
    run(&["sync", &path.to_string_lossy()], None).context("codegraph sync failed")
}

/// Run `codegraph status <path>`.
pub fn status(path: &Path) -> Result<String> {
    run(&["status", &path.to_string_lossy()], None).context("codegraph status failed")
}

// ---------------------------------------------------------------------------
// Query operations (all scoped to `project_path`)
// ---------------------------------------------------------------------------

/// Search for symbols by name/pattern.
pub fn query(
    project_path: &Path,
    search: &str,
    kind: Option<&str>,
    limit: usize,
) -> Result<String> {
    let limit_s = limit.to_string();
    let mut args = vec!["query", search, "--json", "--limit", &limit_s];
    let kind_arg;
    if let Some(k) = kind {
        kind_arg = format!("--kind={k}");
        args.push(&kind_arg);
    }
    run(&args, Some(project_path)).context("codegraph query failed")
}

/// Find all callers of a symbol.
pub fn callers(project_path: &Path, symbol: &str, limit: usize) -> Result<String> {
    let limit_s = limit.to_string();
    run(
        &["callers", symbol, "--json", "--limit", &limit_s],
        Some(project_path),
    )
    .context("codegraph callers failed")
}

/// Find all callees of a symbol.
pub fn callees(project_path: &Path, symbol: &str, limit: usize) -> Result<String> {
    let limit_s = limit.to_string();
    run(
        &["callees", symbol, "--json", "--limit", &limit_s],
        Some(project_path),
    )
    .context("codegraph callees failed")
}

/// Get AI-optimised context for a natural-language task description.
pub fn context(project_path: &Path, task: &str) -> Result<String> {
    run(&["context", task], Some(project_path)).context("codegraph context failed")
}

/// Like `context` but requests JSON output with bounded node count and no inline
/// code blocks — suited for subsequent reranking.  Returns the raw JSON string.
pub fn context_json(project_path: &Path, task: &str, max_nodes: usize) -> Result<String> {
    let max_nodes_s = max_nodes.to_string();
    run(
        &[
            "context",
            task,
            "--format",
            "json",
            "--max-nodes",
            &max_nodes_s,
            "--no-code",
        ],
        Some(project_path),
    )
    .context("codegraph context (json) failed")
}

/// Analyse the impact (downstream dependents) of changing a symbol.
pub fn impact(project_path: &Path, symbol: &str, depth: usize) -> Result<String> {
    let depth_s = depth.to_string();
    run(
        &["impact", symbol, "--json", "--depth", &depth_s],
        Some(project_path),
    )
    .context("codegraph impact failed")
}

/// List files tracked by the index.
pub fn files(project_path: &Path, filter: Option<&str>) -> Result<String> {
    let mut args = vec!["files", "--json"];
    if let Some(f) = filter {
        args.extend(["--filter", f]);
    }
    run(&args, Some(project_path)).context("codegraph files failed")
}

/// Query the installed codegraph version string.
///
/// Returns `"unknown"` on failure so callers never need to abort.
pub fn version() -> String {
    let exe = match codegraph_exe() {
        Ok(e) => e,
        Err(_) => return "unknown".to_owned(),
    };
    std::process::Command::new(&exe)
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| {
            let s = String::from_utf8_lossy(&o.stdout);
            // typical output: "codegraph 0.9.7" — grab the last whitespace-separated token
            s.split_whitespace().last().map(str::to_owned)
        })
        .unwrap_or_else(|| "unknown".to_owned())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Return the path to the codegraph SQLite database for a project.
pub fn db_path(project_path: &Path) -> PathBuf {
    project_path.join(".codegraph").join("codegraph.db")
}

/// Return true if the project has an existing codegraph index.
pub fn is_indexed(project_path: &Path) -> bool {
    db_path(project_path).exists()
}

// ---------------------------------------------------------------------------
// Direct SQLite queries against codegraph.db
// ---------------------------------------------------------------------------

/// A node record read directly from the `nodes` table in `codegraph.db`.
#[derive(Debug, Clone)]
pub struct NodeRecord {
    pub name: String,
    pub qualified_name: String,
    pub file_path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub kind: String,
    pub signature: Option<String>,
    pub docstring: Option<String>,
}

/// Open a read-only connection to `codegraph.db` at `db_path`.
fn open_db(db_path: &Path) -> Result<rusqlite::Connection> {
    rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("Cannot open codegraph.db at {}", db_path.display()))
}

fn row_to_node(row: &rusqlite::Row<'_>) -> rusqlite::Result<NodeRecord> {
    Ok(NodeRecord {
        name: row.get(0)?,
        qualified_name: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
        file_path: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
        start_line: row.get::<_, Option<i64>>(3)?.unwrap_or(0) as u32,
        end_line: row.get::<_, Option<i64>>(4)?.unwrap_or(0) as u32,
        kind: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
        signature: row.get(6)?,
        docstring: row.get(7)?,
    })
}

/// Query `codegraph.db` for an exact symbol match by name or qualified name.
/// Prefers exact `qualified_name` match, then exact `name` match.
pub fn db_query_symbol(db_path: &Path, symbol: &str) -> Result<Option<NodeRecord>> {
    if !db_path.exists() {
        return Ok(None);
    }
    let conn = open_db(db_path)?;
    // Try qualified_name first (exact), then name (exact), then qualified_name suffix.
    let sql = "\
        SELECT name, qualified_name, file_path, start_line, end_line, kind, signature, docstring \
        FROM nodes \
        WHERE qualified_name = ?1 OR name = ?1 \
        ORDER BY CASE WHEN qualified_name = ?1 THEN 0 ELSE 1 END, \
                 (end_line - start_line) ASC \
        LIMIT 1";
    let result = conn
        .query_row(sql, rusqlite::params![symbol], row_to_node)
        .optional()
        .with_context(|| format!("db_query_symbol failed for '{symbol}'"))?;
    Ok(result)
}

/// Query `codegraph.db` for the tightest symbol whose range encloses `line`
/// in `file`. `file` is matched as a path suffix (e.g. `src/foo.rs` matches
/// `some/prefix/src/foo.rs`).
pub fn db_query_at_location(db_path: &Path, file: &str, line: u32) -> Result<Option<NodeRecord>> {
    if !db_path.exists() {
        return Ok(None);
    }
    let conn = open_db(db_path)?;
    // Build two variants of the path suffix pattern.
    let suffix_slash = format!("%/{file}");
    let suffix_backslash = format!("%\\{file}");
    let sql = "\
        SELECT name, qualified_name, file_path, start_line, end_line, kind, signature, docstring \
        FROM nodes \
        WHERE (file_path = ?1 OR file_path LIKE ?2 OR file_path LIKE ?3) \
          AND start_line <= ?4 \
          AND end_line   >= ?4 \
        ORDER BY (end_line - start_line) ASC \
        LIMIT 1";
    let result = conn
        .query_row(
            sql,
            rusqlite::params![file, suffix_slash, suffix_backslash, line],
            row_to_node,
        )
        .optional()
        .with_context(|| format!("db_query_at_location failed for '{file}:{line}'"))?;
    Ok(result)
}

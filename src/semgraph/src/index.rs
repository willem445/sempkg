//! Multi-root indexing orchestration (issues #78 Phase 2a, #79).
//!
//! [`index_roots`] walks one or more source roots, parses every supported file
//! in parallel with rayon, and writes a single schema-v4 `codegraph.db` through
//! one [`crate::writer::GraphWriter`] (single-writer, one transaction). This is
//! the proper fix for #79: multiple `-s`/`--source-dir` roots now land in **one**
//! database instead of the last root silently overwriting the rest.
//!
//! ## File-path representation (the #79 decision)
//!
//! Every node/file `file_path` is stored as a forward-slash relative path. The
//! representation is chosen to be **unambiguous across roots** while staying
//! **consistent with how consumers resolve paths back to disk**
//! (`sembundle`'s `extract_chunks_from_codegraph_db`, and `read_symbol`/
//! `read_code`, which resolve a stored path by joining it onto a source root):
//!
//! - **Single root** → paths are relative to that root, e.g. `python/main.py`.
//!   This is byte-for-byte what CodeGraph emits, so existing single-root
//!   consumers are unaffected.
//! - **Multiple roots** → each root is given a **namespace** that is the
//!   shortest trailing path suffix distinguishing it from the other roots
//!   (usually just its basename; more components are added only when basenames
//!   collide, e.g. `-s backend/src -s frontend/src` → `backend/src` /
//!   `frontend/src`). A file's stored path is `"<namespace>/<relative>"`, which
//!   is globally unique.
//!
//! To map a stored path back to `(root, relative)`, a consumer calls
//! [`resolve_stored_path`]: it re-derives the same namespaces and picks the root
//! whose namespace is the longest leading-component prefix of the stored path,
//! then joins the remainder onto that root. Overlapping/nested roots (one root a
//! filesystem ancestor of another) are not supported and are rejected by
//! [`index_roots`].

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rayon::prelude::*;
use rusqlite::Connection;
use walkdir::{DirEntry, WalkDir};

use crate::model::Language;
use crate::parse::{error_extract, extract, FileExtract};
use crate::resolve::SymbolTable;
use crate::writer::GraphWriter;
use crate::{Error, Result};

/// Directory names never descended into during indexing.
const DEFAULT_IGNORES: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "target",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    "dist",
    "build",
    ".next",
    ".cargo",
];

/// Options controlling a [`index_roots`] run.
#[derive(Debug, Clone, Default)]
pub struct IndexOptions {
    /// Extra directories to exclude (matched as path prefixes of a root's files).
    pub exclude_dirs: Vec<PathBuf>,
}

/// Summary of an indexing run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexStats {
    pub file_count: usize,
    pub node_count: usize,
    pub edge_count: usize,
}

/// Index one or more source `roots` into a single schema-v4 database at
/// `db_path`, overwriting any existing file there.
pub fn index_roots(
    roots: &[PathBuf],
    db_path: &Path,
    options: &IndexOptions,
) -> Result<IndexStats> {
    if roots.is_empty() {
        return Err(Error::Invalid {
            path: db_path.display().to_string(),
            detail: "index_roots requires at least one source root".to_string(),
        });
    }
    let roots = dedupe_roots(roots);
    reject_nested_roots(&roots)?;
    let namespaces = namespaces_for_roots(&roots);
    let now = now_millis();

    let work = enumerate_files(&roots, &namespaces, options);
    let extracts = parse_work(&work, now);

    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let mut files = Vec::new();
    let mut sites = Vec::new();
    for e in extracts {
        nodes.extend(e.nodes);
        edges.extend(e.edges);
        files.push(e.file_record);
        sites.extend(e.sites);
    }

    // Pass 2 (Phase 2b): resolve every reference site against the global symbol
    // table and append the resulting calls/references/imports/instantiates
    // edges to the structural `contains` edges from pass 1.
    let resolved = SymbolTable::build(&nodes).resolve_all(&sites);
    edges.extend(resolved);

    let mut writer = GraphWriter::create(db_path)?;
    writer.write(&nodes, &edges, &files)?;
    writer.finalize()?;

    // Report the *actual* persisted counts. These can be lower than the raw
    // `nodes.len()`/`edges.len()` because the writer's `INSERT OR IGNORE` dedups
    // nodes that share an id (`kind`+`qualified_name`+`file_path`) — distinct
    // definitions that collapse to one node under CodeGraph's id scheme. Reading
    // them back keeps `index_roots` and [`sync`] reporting the same metric, so a
    // sync is provably equal to a from-scratch index (see the sync tests).
    let conn = Connection::open(db_path).map_err(|source| Error::Open {
        path: db_path.display().to_string(),
        source,
    })?;
    current_stats(&conn)
}

/// One unit of parse work: (absolute path on disk, stored db path, language,
/// mtime in epoch millis).
type Work = (PathBuf, String, Language, i64);

/// Enumerate every supported file across all `roots`, computing each file's
/// stored (namespaced, forward-slash) path. Shared by [`index_roots`] and
/// [`sync`] so both see identical stored paths.
fn enumerate_files(roots: &[PathBuf], namespaces: &[String], options: &IndexOptions) -> Vec<Work> {
    let mut work: Vec<Work> = Vec::new();
    for (i, root) in roots.iter().enumerate() {
        let ns = &namespaces[i];
        for entry in WalkDir::new(root)
            .into_iter()
            .filter_entry(|e| !is_ignored(e, &options.exclude_dirs))
        {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let Some(lang) = Language::from_path(path) else {
                continue;
            };
            let rel = path.strip_prefix(root).unwrap_or(path);
            let rel_str = to_forward_slash(rel);
            if rel_str.is_empty() {
                continue;
            }
            let stored = if ns.is_empty() {
                rel_str
            } else {
                format!("{ns}/{rel_str}")
            };
            let mtime = mtime_millis(&entry);
            work.push((path.to_path_buf(), stored, lang, mtime));
        }
    }
    work
}

/// Parse a work list in parallel into [`FileExtract`]s. A file that can't be
/// read as UTF-8 (or at all) is not dropped: it gets a `files` row with `errors`
/// populated (see #78 review F6).
fn parse_work(work: &[Work], now: i64) -> Vec<FileExtract> {
    work.par_iter()
        .map(|(path, stored, lang, mtime)| match std::fs::read(path) {
            Ok(bytes) => match std::str::from_utf8(&bytes) {
                Ok(src) => extract(src, stored, *lang, *mtime, now),
                Err(_) => error_extract(
                    stored,
                    &bytes,
                    *lang,
                    *mtime,
                    now,
                    "file is not valid UTF-8",
                ),
            },
            Err(e) => error_extract(
                stored,
                &[],
                *lang,
                *mtime,
                now,
                &format!("cannot read file: {e}"),
            ),
        })
        .collect()
}

/// Incrementally bring the database at `db_path` up to date with `roots`,
/// re-parsing only files whose content changed, are new, or were deleted.
///
/// The `files.content_hash` (SHA-256, written by pass 1) is the change anchor:
/// an unchanged file hashes the same and is skipped. The resulting database is
/// **canonically equal** to a fresh [`index_roots`] of the same tree (identical
/// nodes/edges/files, modulo `edges.id` autoincrement and `*_at` timestamps).
///
/// ## Invalidation set (the correctness core)
///
/// A file's edges are recomputed when the file changed/was added, **and** when
/// the delta invalidates edges that point *into* it:
///
/// 1. **Nodes** of changed and deleted files are deleted; the `edges` FK
///    `ON DELETE CASCADE` removes every edge incident to them — including
///    resolved edges *from unchanged files into* a changed/deleted file's
///    symbols (the "edges into its symbols" case).
/// 2. Those cross-file edges must be re-created, which needs their **source**
///    files' reference sites. So the re-resolution set is
///    `changed ∪ added ∪ affected`, where `affected` = unchanged files that had
///    a resolved edge whose target lived in a changed/deleted file. Only these
///    unchanged files are re-parsed (for their sites); their nodes/`contains`
///    edges are untouched.
/// 3. A changed file may rename/move a symbol, changing its node id (the id
///    hashes `qualified_name`+`file_path`); resolving the affected sources
///    against the rebuilt global table re-points them to the new id, exactly as
///    a from-scratch index would.
///
/// **Documented boundary:** a purely *additive* change that makes a previously
/// unique global name ambiguous for an unchanged file with no prior edge into
/// the delta is not re-resolved incrementally (that unchanged file is not in
/// `affected`). This is the one case where `sync` can lag a from-scratch index;
/// callers needing exactness after such a change should re-run [`index_roots`].
/// The common dependency-directed edits (modify/add/delete/rename a file others
/// call into) are always exact.
pub fn sync(roots: &[PathBuf], db_path: &Path, options: &IndexOptions) -> Result<IndexStats> {
    if roots.is_empty() {
        return Err(Error::Invalid {
            path: db_path.display().to_string(),
            detail: "sync requires at least one source root".to_string(),
        });
    }
    // No database yet → a sync is just a full index.
    if !db_path.exists() {
        return index_roots(roots, db_path, options);
    }

    let roots = dedupe_roots(roots);
    reject_nested_roots(&roots)?;
    let namespaces = namespaces_for_roots(&roots);
    let now = now_millis();

    let work = enumerate_files(&roots, &namespaces, options);
    let disk: HashMap<String, &Work> = work.iter().map(|w| (w.1.clone(), w)).collect();

    let mut conn = Connection::open(db_path).map_err(|source| Error::Open {
        path: db_path.display().to_string(),
        source,
    })?;
    conn.pragma_update(None, "foreign_keys", true)?;

    // Existing files: stored path → content_hash.
    let db_hashes = read_file_hashes(&conn)?;

    // Classify each stored path.
    let mut changed: Vec<&Work> = Vec::new();
    let mut added: Vec<&Work> = Vec::new();
    for (stored, w) in &disk {
        match db_hashes.get(stored) {
            None => added.push(w),
            Some(old_hash) => {
                let bytes = std::fs::read(&w.0).unwrap_or_default();
                if &crate::model::content_hash(&bytes) != old_hash {
                    changed.push(w);
                }
            }
        }
    }
    let deleted: Vec<String> = db_hashes
        .keys()
        .filter(|p| !disk.contains_key(*p))
        .cloned()
        .collect();

    if changed.is_empty() && added.is_empty() && deleted.is_empty() {
        // Nothing to do; report current totals.
        return current_stats(&conn);
    }

    // Files whose *nodes* leave the DB (changed content or deleted from disk).
    let mut gone: HashSet<String> = deleted.iter().cloned().collect();
    for w in &changed {
        gone.insert(w.1.clone());
    }

    // Re-parse changed ∪ added (full extraction) up front — we need the *new*
    // symbol names to compute the invalidation set precisely.
    let reparse: Vec<Work> = changed
        .iter()
        .chain(added.iter())
        .map(|w| (*w).clone())
        .collect();
    let fresh = parse_work(&reparse, now);

    // `delta_names` = every symbol name whose *global multiplicity* may have
    // changed: names disappearing from `gone` files (read from the DB before we
    // delete them) plus names introduced by the fresh parse. A name's move from
    // unique→ambiguous (or back) shifts the confidence of edges that resolve to
    // it, so any unchanged file resolving such a name must be recomputed.
    let mut delta_names: HashSet<String> = names_in_files(&conn, &gone)?;
    for e in &fresh {
        for n in &e.nodes {
            if n.kind != "file" && n.kind != "import" {
                delta_names.insert(n.name.clone());
            }
        }
    }

    // `affected` = unchanged files (not `gone`) that hold a resolved edge whose
    // target lives in a `gone` file (its edge was cascade-deleted) OR whose
    // target *name* is in `delta_names` (its resolution/confidence may shift).
    // We re-parse these for their sites only; their nodes/`contains` stay put.
    let affected = affected_unchanged_files(&conn, &gone, &delta_names)?;

    // Delete nodes of gone files (cascades their incident edges), and the gone
    // files' `files` rows.
    {
        let tx = conn.transaction()?;
        for path in gone.iter() {
            tx.execute("DELETE FROM nodes WHERE file_path = ?1", [path])?;
            tx.execute("DELETE FROM files WHERE path = ?1", [path])?;
        }
        // Delete the surviving resolved out-edges of `affected` files so we can
        // recompute them cleanly (their edges into gone files were already
        // cascade-deleted; this removes the rest, e.g. edges into unchanged
        // files, avoiding duplicates on re-insert).
        for path in &affected {
            tx.execute(
                "DELETE FROM edges WHERE kind != 'contains' AND source IN \
                 (SELECT id FROM nodes WHERE file_path = ?1)",
                [path],
            )?;
        }
        tx.commit()?;
    }

    let affected_work: Vec<Work> = affected
        .iter()
        .filter_map(|p| disk.get(p).map(|w| (*w).clone()))
        .collect();
    let affected_extracts = parse_work(&affected_work, now);

    // Insert fresh nodes/contains/files.
    let mut new_nodes = Vec::new();
    let mut new_contains = Vec::new();
    let mut new_files = Vec::new();
    let mut sites = Vec::new();
    for e in fresh {
        new_nodes.extend(e.nodes);
        new_contains.extend(e.edges);
        new_files.push(e.file_record);
        sites.extend(e.sites);
    }
    // Affected files contribute only their sites (nodes/contains unchanged).
    for e in affected_extracts {
        sites.extend(e.sites);
    }

    {
        let mut writer = crate::writer::GraphWriter::from_connection(conn);
        writer.write(&new_nodes, &new_contains, &new_files)?;
        // Rebuild the symbol table over the *current* full node set and resolve
        // the sites of all re-parsed (reparse ∪ affected) files.
        let all_nodes = writer.all_nodes()?;
        let resolved = SymbolTable::build(&all_nodes).resolve_all(&sites);
        writer.write(&[], &resolved, &[])?;
        writer.finalize()?;
    }

    // Reopen to report the final totals.
    let conn = Connection::open(db_path).map_err(|source| Error::Open {
        path: db_path.display().to_string(),
        source,
    })?;
    current_stats(&conn)
}

/// Read `files.path → content_hash` from an existing database.
fn read_file_hashes(conn: &Connection) -> Result<HashMap<String, String>> {
    let mut stmt = conn.prepare("SELECT path, content_hash FROM files")?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .filter_map(|r| r.ok());
    Ok(rows.collect())
}

/// The distinct symbol names (excluding `file`/`import` nodes) defined in the
/// given file set — read before those files' nodes are deleted, so their
/// disappearing names can enter `delta_names`.
fn names_in_files(conn: &Connection, files: &HashSet<String>) -> Result<HashSet<String>> {
    if files.is_empty() {
        return Ok(HashSet::new());
    }
    let mut stmt = conn
        .prepare("SELECT name FROM nodes WHERE kind NOT IN ('file','import') AND file_path = ?1")?;
    let mut names = HashSet::new();
    for path in files {
        let rows = stmt
            .query_map([path], |r| r.get::<_, String>(0))?
            .filter_map(|r| r.ok());
        names.extend(rows);
    }
    Ok(names)
}

/// Unchanged files that must be re-resolved because the delta invalidated one
/// of their edges: an unchanged file (source not in `gone`) holding a resolved
/// (non-`contains`) edge whose **target** either lives in a `gone` file (the
/// edge was cascade-deleted) or whose **target name** is in `delta_names` (the
/// name's global multiplicity may have shifted, changing the edge's resolution
/// or confidence). This is the reverse-dependency blast radius of the change.
fn affected_unchanged_files(
    conn: &Connection,
    gone: &HashSet<String>,
    delta_names: &HashSet<String>,
) -> Result<Vec<String>> {
    if gone.is_empty() && delta_names.is_empty() {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT DISTINCT s.file_path, t.file_path, t.name \
         FROM edges e \
         JOIN nodes s ON s.id = e.source \
         JOIN nodes t ON t.id = e.target \
         WHERE e.kind != 'contains'",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .filter_map(|r| r.ok());

    let mut out: HashSet<String> = HashSet::new();
    for (src_file, tgt_file, tgt_name) in rows {
        if gone.contains(&src_file) {
            continue;
        }
        if gone.contains(&tgt_file) || delta_names.contains(&tgt_name) {
            out.insert(src_file);
        }
    }
    let mut v: Vec<String> = out.into_iter().collect();
    v.sort();
    Ok(v)
}

/// Summary counts for the database behind `conn`.
fn current_stats(conn: &Connection) -> Result<IndexStats> {
    let (files, nodes, edges) = conn.query_row(
        "SELECT (SELECT COUNT(*) FROM files), (SELECT COUNT(*) FROM nodes), \
                (SELECT COUNT(*) FROM edges)",
        [],
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        },
    )?;
    Ok(IndexStats {
        file_count: files as usize,
        node_count: nodes as usize,
        edge_count: edges as usize,
    })
}

/// Current time in epoch milliseconds (shared by the writer for `updated_at`).
pub(crate) fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn mtime_millis(entry: &DirEntry) -> i64 {
    entry
        .metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn is_ignored(entry: &DirEntry, exclude_dirs: &[PathBuf]) -> bool {
    if entry.file_type().is_dir() {
        if let Some(name) = entry.file_name().to_str() {
            if DEFAULT_IGNORES.contains(&name) {
                return true;
            }
        }
    }
    let path = entry.path();
    exclude_dirs.iter().any(|ex| path.starts_with(ex))
}

/// Render a relative path with forward slashes, dropping `.`/`..`/root parts.
fn to_forward_slash(path: &Path) -> String {
    let parts: Vec<String> = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => s.to_str().map(String::from),
            _ => None,
        })
        .collect();
    parts.join("/")
}

/// Normalized path components of a root (drops root/`.` parts, keeps a `C:`
/// prefix and `..`), used for namespace derivation.
fn root_components(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|c| match c {
            Component::Normal(s) => s.to_str().map(String::from),
            Component::Prefix(p) => Some(p.as_os_str().to_string_lossy().into_owned()),
            Component::ParentDir => Some("..".to_string()),
            _ => None,
        })
        .collect()
}

/// Drop roots that normalize to the same component sequence (keeping first).
fn dedupe_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for r in roots {
        if seen.insert(root_components(r)) {
            out.push(r.clone());
        }
    }
    out
}

/// Reject overlapping roots where one is a filesystem ancestor of another —
/// their namespaced paths would be ambiguous.
fn reject_nested_roots(roots: &[PathBuf]) -> Result<()> {
    let comps: Vec<Vec<String>> = roots.iter().map(|r| root_components(r)).collect();
    for (i, a) in comps.iter().enumerate() {
        for (j, b) in comps.iter().enumerate() {
            if i != j && !a.is_empty() && b.len() > a.len() && b[..a.len()] == a[..] {
                return Err(Error::Invalid {
                    path: roots[j].display().to_string(),
                    detail: format!(
                        "source root '{}' is nested inside '{}'; nested roots are not supported",
                        roots[j].display(),
                        roots[i].display()
                    ),
                });
            }
        }
    }
    Ok(())
}

/// Compute a unique namespace per root (see the module docs). A single root
/// gets the empty namespace (no prefix, CodeGraph-compatible).
pub fn namespaces_for_roots(roots: &[PathBuf]) -> Vec<String> {
    if roots.len() <= 1 {
        return vec![String::new(); roots.len()];
    }
    let comps: Vec<Vec<String>> = roots.iter().map(|r| root_components(r)).collect();
    let max_len = comps.iter().map(|c| c.len()).max().unwrap_or(0).max(1);

    for k in 1..=max_len {
        let suffixes: Vec<String> = comps
            .iter()
            .map(|c| c[c.len().saturating_sub(k)..].join("/"))
            .collect();
        if all_distinct(&suffixes) {
            return suffixes;
        }
    }

    // Fallback: identical (or unresolvable) roots — disambiguate with an index.
    comps
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{}#{i}", c.join("/")))
        .collect()
}

fn all_distinct(items: &[String]) -> bool {
    let mut seen = HashSet::with_capacity(items.len());
    items.iter().all(|s| seen.insert(s.as_str()))
}

/// Reverse of the stored-path representation: map a database `file_path` back to
/// the `(root_index, relative_path)` it came from, given the same `roots` list
/// used to build the database. The caller reads the file at
/// `roots[index].join(relative_path)`.
///
/// Returns `None` if no root's namespace prefixes `stored` (e.g. a path from a
/// different root set).
pub fn resolve_stored_path(roots: &[PathBuf], stored: &str) -> Option<(usize, PathBuf)> {
    let roots = dedupe_roots(roots);
    let namespaces = namespaces_for_roots(&roots);
    let stored_comps: Vec<&str> = stored.split('/').filter(|s| !s.is_empty()).collect();

    // Single-root (empty namespace): the stored path is already root-relative.
    if namespaces.len() == 1 && namespaces[0].is_empty() {
        return Some((0, PathBuf::from(stored_comps.join("/"))));
    }

    let mut best: Option<(usize, usize)> = None;
    for (i, ns) in namespaces.iter().enumerate() {
        let ns_comps: Vec<&str> = ns.split('/').filter(|s| !s.is_empty()).collect();
        if ns_comps.is_empty() || stored_comps.len() <= ns_comps.len() {
            continue;
        }
        if stored_comps[..ns_comps.len()] == ns_comps[..]
            && best.is_none_or(|(_, len)| ns_comps.len() > len)
        {
            best = Some((i, ns_comps.len()));
        }
    }
    best.map(|(i, len)| (i, PathBuf::from(stored_comps[len..].join("/"))))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pb(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn single_root_has_empty_namespace() {
        assert_eq!(namespaces_for_roots(&[pb("proj/src")]), vec![String::new()]);
    }

    #[test]
    fn distinct_basenames_namespace_by_basename() {
        let roots = [pb("proj/src"), pb("proj/radar_src")];
        assert_eq!(namespaces_for_roots(&roots), vec!["src", "radar_src"]);
    }

    #[test]
    fn colliding_basenames_extend_to_shortest_distinguishing_suffix() {
        // Same basename `src` under different parents → include one more level.
        let roots = [pb("backend/src"), pb("frontend/src")];
        assert_eq!(
            namespaces_for_roots(&roots),
            vec!["backend/src", "frontend/src"]
        );
    }

    #[test]
    fn resolve_round_trips_distinct_basenames() {
        let roots = [pb("proj/src"), pb("proj/radar_src")];
        // A file under the second root is stored as `radar_src/<rel>`.
        assert_eq!(
            resolve_stored_path(&roots, "radar_src/mod/a.rs"),
            Some((1, pb("mod/a.rs")))
        );
        assert_eq!(
            resolve_stored_path(&roots, "src/lib.rs"),
            Some((0, pb("lib.rs")))
        );
    }

    #[test]
    fn resolve_round_trips_colliding_basenames() {
        let roots = [pb("backend/src"), pb("frontend/src")];
        assert_eq!(
            resolve_stored_path(&roots, "backend/src/a.rs"),
            Some((0, pb("a.rs")))
        );
        assert_eq!(
            resolve_stored_path(&roots, "frontend/src/b/c.rs"),
            Some((1, pb("b/c.rs")))
        );
    }

    #[test]
    fn resolve_single_root_is_identity_relative() {
        let roots = [pb("proj/src")];
        assert_eq!(
            resolve_stored_path(&roots, "python/main.py"),
            Some((0, pb("python/main.py")))
        );
    }

    #[test]
    fn resolve_unknown_prefix_is_none() {
        let roots = [pb("proj/src"), pb("proj/radar_src")];
        assert_eq!(resolve_stored_path(&roots, "elsewhere/x.rs"), None);
    }

    #[test]
    fn nested_roots_are_rejected() {
        let roots = dedupe_roots(&[pb("proj"), pb("proj/src")]);
        assert!(reject_nested_roots(&roots).is_err());
    }

    #[test]
    fn identical_roots_dedupe() {
        assert_eq!(dedupe_roots(&[pb("a/b"), pb("a/b")]).len(), 1);
    }
}

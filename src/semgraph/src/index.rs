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

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rayon::prelude::*;
use walkdir::{DirEntry, WalkDir};

use crate::model::Language;
use crate::parse::{extract, FileExtract};
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

    // Enumerate every supported file across all roots, computing its stored path.
    let mut work: Vec<(PathBuf, String, Language, i64)> = Vec::new();
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

    // Parse in parallel; skip files that can't be read as UTF-8 (binaries, etc.).
    let extracts: Vec<FileExtract> = work
        .par_iter()
        .filter_map(|(path, stored, lang, mtime)| {
            let src = std::fs::read_to_string(path).ok()?;
            Some(extract(&src, stored, *lang, *mtime, now))
        })
        .collect();

    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let mut files = Vec::new();
    for e in extracts {
        nodes.extend(e.nodes);
        edges.extend(e.edges);
        files.push(e.file_record);
    }

    let mut writer = GraphWriter::create(db_path)?;
    writer.write(&nodes, &edges, &files)?;
    writer.finalize()?;

    Ok(IndexStats {
        file_count: files.len(),
        node_count: nodes.len(),
        edge_count: edges.len(),
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
